//! csv-wasm: a WASM SIMD reader for the csv-reader-speed bounty.
//!
//! It answers exactly what csv-reader 1.0.0 answers for an input {text, header}:
//! {columns, rows, errors}. The text is scanned once, left to right, as UTF-8
//! bytes (every delimiter is ASCII, so multi-byte characters can never match
//! one). Fields are kept as spans of the input wherever the value needs no
//! change, and built into a scratch buffer only where it does (a doubled
//! quote, a CR inside quotes, a stray CR, junk after a closing quote).
//!
//! The JSON answer is built straight into an output buffer: no intermediate
//! value tree, no per-field allocation on the common path.
//!
//! Safety: the SIMD scans load 16 bytes at a time and may read up to 15 bytes
//! past the end of the input. Every input must therefore live in a buffer
//! with at least 16 zero bytes past its end; `alloc_input` is the only way
//! to make one, and it always reserves them. The padding bytes are zero, so
//! they never match a delimiter and never count as a newline.

use std::arch::wasm32::*;

const COMMA: u8 = b',';
const QUOTE: u8 = b'"';
const LF: u8 = b'\n';
const CR: u8 = b'\r';

/// How a field ended: at a comma, at a line break, or at the end of the text.
const END_COMMA: u8 = 0;
const END_BREAK: u8 = 1;
const END_EOF: u8 = 2;

/// First byte at or after `pos` that is `,`, LF or CR; `t.len()` when there is none.
#[inline]
fn find_delim3(t: &[u8], mut pos: usize) -> usize {
    let len = t.len();
    let base = t.as_ptr();
    while pos < len {
        unsafe {
            let chunk = v128_load(base.add(pos) as *const v128);
            let m = v128_or(
                v128_or(
                    i8x16_eq(chunk, i8x16_splat(COMMA as i8)),
                    i8x16_eq(chunk, i8x16_splat(LF as i8)),
                ),
                i8x16_eq(chunk, i8x16_splat(CR as i8)),
            );
            let bits = i8x16_bitmask(m);
            if bits != 0 {
                let idx = pos + bits.trailing_zeros() as usize;
                return if idx < len { idx } else { len };
            }
        }
        pos += 16;
    }
    len
}

/// First `"` at or after `pos` (or `t.len()`), with the newlines counted in the
/// bytes scanned before it: returns (quote position, newline count, has CR).
/// A newline is an LF, a lone CR, or a CRLF pair: a CRLF counts once.
#[inline]
fn find_quote_count(t: &[u8], mut pos: usize) -> (usize, u32, bool) {
    let len = t.len();
    let base = t.as_ptr();
    let mut nl: u32 = 0;
    let mut has_cr = false;
    // Whether the byte just before this chunk was a CR: a LF opening the
    // chunk then completes a CRLF pair with it, which counts once, not twice.
    let mut prev_cr = false;
    while pos < len {
        unsafe {
            let chunk = v128_load(base.add(pos) as *const v128);
            let qm = i8x16_bitmask(i8x16_eq(chunk, i8x16_splat(QUOTE as i8)));
            if qm != 0 {
                let k = qm.trailing_zeros() as usize;
                let mask = if k >= 16 { u16::MAX } else { (1u16 << k) - 1 };
                let nlm = i8x16_bitmask(i8x16_eq(chunk, i8x16_splat(LF as i8))) & mask;
                let crm = i8x16_bitmask(i8x16_eq(chunk, i8x16_splat(CR as i8))) & mask;
                let pairs = ((crm << 1) & nlm).count_ones();
                let boundary = (prev_cr && (nlm & 1) != 0) as u32;
                nl += nlm.count_ones() + crm.count_ones() - pairs - boundary;
                if crm != 0 {
                    has_cr = true;
                }
                return (pos + k, nl, has_cr);
            }
            let nlm = i8x16_bitmask(i8x16_eq(chunk, i8x16_splat(LF as i8)));
            let crm = i8x16_bitmask(i8x16_eq(chunk, i8x16_splat(CR as i8)));
            let pairs = ((crm << 1) & nlm).count_ones();
            let boundary = (prev_cr && (nlm & 1) != 0) as u32;
            nl += nlm.count_ones() + crm.count_ones() - pairs - boundary;
            if crm != 0 {
                has_cr = true;
            }
            prev_cr = (crm >> 15) != 0;
        }
        pos += 16;
    }
    (len, nl, has_cr)
}

/// Serialize one record's fields into `out` as a JSON array, padding with
/// empty strings or cutting to `width` (the caller already recorded the
/// matching error).
fn write_record_json(fields: &[Field], t: &[u8], scratch: &[u8], out: &mut Vec<u8>, width: usize) {
    out.push(b'[');
    for i in 0..width {
        if i > 0 {
            out.push(b',');
        }
        out.push(b'"');
        if i < fields.len() {
            match fields[i] {
                Field::Span(a, l) => {
                    let a = a as usize;
                    let l = l as usize;
                    if span_needs_escape(t, a, l) {
                        esc_push(out, &t[a..a + l]);
                    } else {
                        out.extend_from_slice(&t[a..a + l]);
                    }
                }
                Field::Owned(a, l) => {
                    let a = a as usize;
                    esc_push(out, &scratch[a..a + l as usize]);
                }
            }
        }
        out.push(b'"');
    }
    out.push(b']');
}

/// One hex digit, lower case.
#[inline]
fn hex_digit(d: u8) -> u8 {
    b"0123456789abcdef"[d as usize & 0xF]
}

/// Whether a span of the input (at `a`, `l` bytes) holds a byte JSON must
/// escape: a control byte, `"` or `\`. One SIMD check; the input carries 16
/// zero bytes of padding past its end, so loading 16 bytes here is safe.
#[inline]
fn span_needs_escape(t: &[u8], a: usize, l: usize) -> bool {
    if l == 0 {
        return false;
    }
    unsafe {
        let chunk = v128_load(t.as_ptr().add(a) as *const v128);
        let m = v128_or(
            v128_or(
                u8x16_lt(chunk, u8x16_splat(0x20)),
                i8x16_eq(chunk, i8x16_splat(b'"' as i8)),
            ),
            i8x16_eq(chunk, i8x16_splat(b'\\' as i8)),
        );
        let bits = i8x16_bitmask(m);
        let mask = if l >= 16 { u16::MAX } else { (1u16 << l) - 1 };
        (bits & mask) != 0
    }
}

/// Append `bytes` to `out` as JSON string content: `"` `\` and every control
/// byte escaped, everything else copied as it stands. A fast path copies a
/// clean run whole.
#[inline]
fn esc_push(out: &mut Vec<u8>, bytes: &[u8]) {
    let mut i = 0;
    let n = bytes.len();
    while i < n {
        let b = bytes[i];
        if b >= 0x20 && b != b'"' && b != b'\\' {
            // A clean run: copy it whole.
            let mut j = i + 1;
            while j < n {
                let c = bytes[j];
                if c < 0x20 || c == b'"' || c == b'\\' {
                    break;
                }
                j += 1;
            }
            out.extend_from_slice(&bytes[i..j]);
            i = j;
            continue;
        }
        match b {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x0C => out.extend_from_slice(b"\\f"),
            _ => {
                out.extend_from_slice(b"\\u00");
                out.push(hex_digit(b >> 4));
                out.push(hex_digit(b));
            }
        }
        i += 1;
    }
}

/// A field's value: a span of the input, or bytes built in the scratch buffer.
#[derive(Clone, Copy)]
enum Field {
    Span(u32, u32),
    Owned(u32, u32),
}

impl Field {
    #[inline]
    fn len(&self) -> usize {
        match *self {
            Field::Span(_, l) => l as usize,
            Field::Owned(_, l) => l as usize,
        }
    }
}

struct Parser<'a> {
    t: &'a [u8],
    pos: usize,
    line: u32,
    header: bool,
    width: i32,
    cols_done: bool,
    cols: Vec<u8>,
    rows: Vec<u8>,
    errs: Vec<u8>,
    nrows: usize,
    scratch: Vec<u8>,
    fields: Vec<Field>,
}

impl<'a> Parser<'a> {
    fn new(t: &'a [u8], header: bool) -> Self {
        Parser {
            t,
            pos: 0,
            line: 1,
            header,
            width: -1,
            cols_done: false,
            cols: Vec::new(),
            rows: Vec::new(),
            errs: Vec::new(),
            nrows: 0,
            scratch: Vec::new(),
            fields: Vec::new(),
        }
    }

    /// One error with no extra keys: {"code","line","row"}.
    #[inline]
    fn err_simple(&mut self, code: &str, line: u32, row: Option<u32>) {
        if !self.errs.is_empty() {
            self.errs.push(b',');
        }
        self.errs.extend_from_slice(b"{\"code\":\"");
        self.errs.extend_from_slice(code.as_bytes());
        self.errs.extend_from_slice(b"\",\"line\":");
        push_u32(&mut self.errs, line);
        self.errs.extend_from_slice(b",\"row\":");
        match row {
            Some(r) => push_u32(&mut self.errs, r),
            None => self.errs.extend_from_slice(b"null"),
        }
        self.errs.push(b'}');
    }

    /// A record's short_row error, with its field count.
    #[inline]
    fn err_short(&mut self, line: u32, row: Option<u32>, fields: usize) {
        if !self.errs.is_empty() {
            self.errs.push(b',');
        }
        self.errs.extend_from_slice(b"{\"code\":\"short_row\",\"line\":");
        push_u32(&mut self.errs, line);
        self.errs.extend_from_slice(b",\"row\":");
        match row {
            Some(r) => push_u32(&mut self.errs, r),
            None => self.errs.extend_from_slice(b"null"),
        }
        self.errs.extend_from_slice(b",\"fields\":");
        push_usize(&mut self.errs, fields);
        self.errs.push(b'}');
    }

    /// A record's long_row error, with the cut fields as JSON strings.
    #[inline]
    fn err_long(&mut self, line: u32, row: Option<u32>, extra: &[Field]) {
        if !self.errs.is_empty() {
            self.errs.push(b',');
        }
        self.errs.extend_from_slice(b"{\"code\":\"long_row\",\"line\":");
        push_u32(&mut self.errs, line);
        self.errs.extend_from_slice(b",\"row\":");
        match row {
            Some(r) => push_u32(&mut self.errs, r),
            None => self.errs.extend_from_slice(b"null"),
        }
        self.errs.extend_from_slice(b",\"extra\":[");
        for (i, f) in extra.iter().enumerate() {
            if i > 0 {
                self.errs.push(b',');
            }
            self.errs.push(b'"');
            self.push_field_json(f);
            self.errs.push(b'"');
        }
        self.errs.extend_from_slice(b"]}");
    }

    /// Append a field's value as JSON string content (no surrounding quotes).
    #[inline]
    fn push_field_json(&mut self, f: &Field) {
        match *f {
            Field::Span(a, l) => {
                let a = a as usize;
                esc_push(&mut self.errs, &self.t[a..a + l as usize]);
            }
            Field::Owned(a, l) => {
                let a = a as usize;
                esc_push(&mut self.errs, &self.scratch[a..a + l as usize]);
            }
        }
    }

    /// Copy a field's raw value bytes into the scratch buffer.
    #[inline]
    fn push_value_bytes(&mut self, f: Field) {
        match f {
            Field::Span(a, l) => {
                let a = a as usize;
                let l = l as usize;
                self.scratch.extend_from_slice(&self.t[a..a + l]);
            }
            Field::Owned(a, l) => {
                let a = a as usize;
                let l = l as usize;
                let dest = self.scratch.len();
                self.scratch.resize(dest + l, 0);
                self.scratch.copy_within(a..a + l, dest);
            }
        }
    }

    /// Scan an unquoted run from `pos`. Returns (end, kept, ended, new_pos):
    /// `end` is where the field's value stops, `kept` how many of its bytes
    /// stay (stray CRs are dropped), `ended` how it ended, `new_pos` where
    /// reading continues. Records stray_cr errors and counts line breaks.
    fn scan_unquoted(
        &mut self,
        line: u32,
        row: Option<u32>,
        mut pos: usize,
    ) -> (usize, usize, u8, usize) {
        let start = pos;
        let mut stray = 0usize;
        loop {
            let d = find_delim3(self.t, pos);
            if d >= self.t.len() {
                let kept = (self.t.len() - start) - stray;
                return (self.t.len(), kept, END_EOF, self.t.len());
            }
            let b = self.t[d];
            if b == COMMA {
                return (d, (d - start) - stray, END_COMMA, d + 1);
            }
            if b == LF {
                self.line += 1;
                return (d, (d - start) - stray, END_BREAK, d + 1);
            }
            if d + 1 < self.t.len() && self.t[d + 1] == LF {
                self.line += 1;
                return (d, (d - start) - stray, END_BREAK, d + 2);
            }
            self.err_simple("stray_cr", line, row);
            stray += 1;
            pos = d + 1;
        }
    }

    /// Read an unquoted field starting at self.pos. `prefix` is the literal
    /// quote a re-read after an unterminated quote starts its value with.
    fn read_unquoted(
        &mut self,
        line: u32,
        row: Option<u32>,
        prefix: Option<u8>,
    ) -> u8 {
        let start = self.pos;
        let (end, kept, ended, newpos) = self.scan_unquoted(line, row, self.pos);
        self.pos = newpos;
        match prefix {
            Some(px) => {
                let s0 = self.scratch.len();
                self.scratch.push(px);
                if kept == end - start {
                    self.scratch.extend_from_slice(&self.t[start..end]);
                } else {
                    for &b in &self.t[start..end] {
                        if b != CR {
                            self.scratch.push(b);
                        }
                    }
                }
                self.fields
                    .push(Field::Owned(s0 as u32, (self.scratch.len() - s0) as u32));
            }
            None => {
                if kept == end - start {
                    self.fields
                        .push(Field::Span(start as u32, (end - start) as u32));
                } else {
                    let s0 = self.scratch.len();
                    for &b in &self.t[start..end] {
                        if b != CR {
                            self.scratch.push(b);
                        }
                    }
                    self.fields
                        .push(Field::Owned(s0 as u32, (self.scratch.len() - s0) as u32));
                }
            }
        }
        ended
    }

    /// Read a quoted field whose opening quote is at self.pos.
    fn read_quoted(&mut self, line: u32, row: Option<u32>) -> u8 {
        let open = self.pos;
        let open_line = self.line;
        let content_start = open + 1;
        let mut pos = content_start;
        let mut has_dd = false;
        let mut has_cr = false;
        let close: usize;
        loop {
            if pos >= self.t.len() {
                self.line = open_line;
                self.err_simple("unterminated_quote", line, row);
                self.pos = open + 1;
                return self.read_unquoted(line, row, Some(QUOTE));
            }
            let (q, nl, cr) = find_quote_count(self.t, pos);
            self.line += nl;
            if cr {
                has_cr = true;
            }
            if q >= self.t.len() {
                self.line = open_line;
                self.err_simple("unterminated_quote", line, row);
                self.pos = open + 1;
                return self.read_unquoted(line, row, Some(QUOTE));
            }
            if q + 1 < self.t.len() && self.t[q + 1] == QUOTE {
                has_dd = true;
                pos = q + 2;
                continue;
            }
            close = q;
            break;
        }
        let value: Field = if !has_dd && !has_cr {
            Field::Span(content_start as u32, (close - content_start) as u32)
        } else {
            let s0 = self.scratch.len();
            let mut i = content_start;
            while i < close {
                let b = self.t[i];
                if b == QUOTE && i + 1 < close && self.t[i + 1] == QUOTE {
                    self.scratch.push(QUOTE);
                    i += 2;
                } else if b == CR {
                    self.scratch.push(LF);
                    i += if i + 1 < close && self.t[i + 1] == LF { 2 } else { 1 };
                } else {
                    self.scratch.push(b);
                    i += 1;
                }
            }
            Field::Owned(s0 as u32, (self.scratch.len() - s0) as u32)
        };
        let mut pos = close + 1;
        if pos >= self.t.len() {
            self.pos = pos;
            self.fields.push(value);
            return END_EOF;
        }
        let c = self.t[pos];
        if c == COMMA {
            self.pos = pos + 1;
            self.fields.push(value);
            return END_COMMA;
        }
        if c == LF {
            self.pos = pos + 1;
            self.line += 1;
            self.fields.push(value);
            return END_BREAK;
        }
        if c == CR && pos + 1 < self.t.len() && self.t[pos + 1] == LF {
            self.pos = pos + 2;
            self.line += 1;
            self.fields.push(value);
            return END_BREAK;
        }
        // What follows the closing quote is appended as it stands.
        let (jend, jkept, jended, jnewpos) = self.scan_unquoted(line, row, pos);
        self.pos = jnewpos;
        if jkept > 0 {
            self.err_simple("quote_junk", line, row);
            let s0 = self.scratch.len();
            self.push_value_bytes(value);
            if jkept == jend - pos {
                self.scratch.extend_from_slice(&self.t[pos..jend]);
            } else {
                for &b in &self.t[pos..jend] {
                    if b != CR {
                        self.scratch.push(b);
                    }
                }
            }
            self.fields
                .push(Field::Owned(s0 as u32, (self.scratch.len() - s0) as u32));
        } else {
            self.fields.push(value);
        }
        jended
    }

}

/// Append a decimal u32.
#[inline]
fn push_u32(out: &mut Vec<u8>, mut v: u32) {
    let mut buf = [0u8; 10];
    let mut i = buf.len();
    if v == 0 {
        out.push(b'0');
        return;
    }
    while v > 0 {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
    }
    out.extend_from_slice(&buf[i..]);
}

/// Append a decimal usize.
#[inline]
fn push_usize(out: &mut Vec<u8>, v: usize) {
    push_u32(out, v as u32);
}

/// Parse one input and write its answer as JSON text into `out`.
fn parse_csv(text: &[u8], header: bool, out: &mut Vec<u8>) {
    let mut p = Parser::new(text, header);
    if text.len() >= 3 && text[0] == 0xEF && text[1] == 0xBB && text[2] == 0xBF {
        p.pos = 3;
    }
    loop {
        // Lines left with no characters are no records.
        while p.pos < text.len() {
            let b = text[p.pos];
            if b == LF {
                p.pos += 1;
                p.line += 1;
            } else if b == CR && p.pos + 1 < text.len() && text[p.pos + 1] == LF {
                p.pos += 2;
                p.line += 1;
            } else {
                break;
            }
        }
        if p.pos >= text.len() {
            break;
        }
        let record_line = p.line;
        let record_row: Option<u32> = if header && !p.cols_done {
            None
        } else {
            Some(p.nrows as u32)
        };
        p.fields.clear();
        p.scratch.clear();
        let mut last_quoted = false;
        loop {
            let quoted = p.pos < text.len() && text[p.pos] == QUOTE;
            last_quoted = quoted;
            let ended = if quoted {
                p.read_quoted(record_line, record_row)
            } else {
                p.read_unquoted(record_line, record_row, None)
            };
            if ended != END_COMMA {
                break;
            }
        }
        // A line that held only stray CRs is left with no characters: no record.
        if p.fields.len() == 1 && p.fields[0].len() == 0 && !last_quoted {
            continue;
        }
        if p.width < 0 {
            p.width = p.fields.len() as i32;
        }
        if header && !p.cols_done {
            p.cols_done = true;
            let mut cols = Vec::with_capacity(64);
            write_record_json(&p.fields, p.t, &p.scratch, &mut cols, p.fields.len());
            p.cols = cols;
            continue;
        }
        let w = p.width as usize;
        if p.fields.len() < w {
            p.err_short(record_line, record_row, p.fields.len());
        } else if p.fields.len() > w {
            let extra: Vec<Field> = p.fields[w..].to_vec();
            p.err_long(record_line, record_row, &extra);
        }
        if !p.rows.is_empty() {
            p.rows.push(b',');
        }
        write_record_json(&p.fields, p.t, &p.scratch, &mut p.rows, w);
        p.nrows += 1;
    }
    out.clear();
    let need = text.len() + text.len() / 4 + 64;
    if out.capacity() < need {
        out.reserve(need - out.len());
    }
    out.extend_from_slice(b"{\"columns\":");
    if header {
        if p.cols_done {
            out.extend_from_slice(&p.cols);
        } else {
            out.extend_from_slice(b"[]");
        }
    } else {
        out.extend_from_slice(b"null");
    }
    out.extend_from_slice(b",\"rows\":[");
    out.extend_from_slice(&p.rows);
    out.extend_from_slice(b"],\"errors\":[");
    out.extend_from_slice(&p.errs);
    out.extend_from_slice(b"]}");
}

/// One input buffer and one output buffer, reused across calls so the
/// linear memory grows once to the largest case and never again. WASM here
/// is single-threaded, so plain statics are safe.
static mut IN_BUF: Vec<u8> = Vec::new();
static mut OUT_BUF: Vec<u8> = Vec::new();

/// Reserve an input buffer of `len` bytes plus 16 zero bytes of padding past
/// its end (the SIMD scans may read that far). Returns its address. The
/// buffer is reused: the caller overwrites the first `len` bytes.
#[no_mangle]
pub extern "C" fn alloc_input(len: usize) -> *mut u8 {
    unsafe {
        let buf = &mut *(&raw mut IN_BUF);
        if buf.len() < len + 16 {
            buf.resize(len + 16, 0);
        }
        buf[len..len + 16].fill(0);
        buf.as_mut_ptr()
    }
}

/// Parse the input at `ptr` (`len` bytes) and return the answer's JSON text:
/// the high 32 bits hold its address, the low 32 its length. The text lives
/// in the reused output buffer: read it before the next call.
#[no_mangle]
pub extern "C" fn parse(ptr: *const u8, len: usize, header: u32) -> i64 {
    let text = unsafe { std::slice::from_raw_parts(ptr, len) };
    unsafe {
        let out = &mut *(&raw mut OUT_BUF);
        parse_csv(text, header != 0, out);
        let out_len = out.len() as i64;
        let out_ptr = out.as_ptr() as i64;
        (out_ptr << 32) | out_len
    }
}
