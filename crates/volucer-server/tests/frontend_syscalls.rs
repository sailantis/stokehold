//! Syscall counts for the FastCGI front-end: how many times reading one request or writing
//! one response reaches the operating system.
//!
//! `volucer-server`'s listener is both a FastCGI server and, on the request side, a client:
//! it reads a request from nginx as a sequence of records and writes a response back the same
//! way. Both directions go through `volucer_fcgi`'s codec, which reads and writes one field at
//! a time, so over a real socket each field is its own `read(2)` or `write(2)`.
//!
//! The counters wrap the codec, so these measure the production call pattern. There is no
//! `#[global_allocator]` here, and the counters are thread-local so the parallel harness cannot
//! pollute them.

use std::cell::Cell;
use std::io::{self, Read, Write};

use volucer_fcgi::{BeginRequest, EndRequest, Header, RecordType};
use volucer_server::frontend::fastcgi::{read_next, write_response};

thread_local! {
    static READS: Cell<usize> = const { Cell::new(0) };
    static WRITES: Cell<usize> = const { Cell::new(0) };
}

/// How much of a request the listener reads ahead of the codec.
///
/// Duplicated from `volucer-server`'s listener rather than exported: the point of the test is
/// to measure the effect of buffering the transport, and the exact capacity only decides how
/// many fills a large request needs.
const READ_BUFFER_LEN: usize = 8 * 1024;

/// A one-direction transport double: writes are counted and kept, reads come from `input`.
#[derive(Default)]
struct Counting {
    input: std::io::Cursor<Vec<u8>>,
    writes: usize,
    /// Everything written, so a test can inspect the framing the write count no longer shows.
    out: Vec<u8>,
}

impl Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        READS.with(|count| count.set(count.get() + 1));
        Read::read(&mut self.input, buf)
    }
}

impl Write for Counting {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        WRITES.with(|count| count.set(count.get() + 1));
        self.writes += 1;
        self.out.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn count_start() {
    let _ = READS.try_with(|count| count.set(0));
    let _ = WRITES.try_with(|count| count.set(0));
}

/// Append one record: header, content, then the padding the header asks for.
///
/// The padding is not optional. `read_record` consumes `padding_length` bytes after the
/// content, so omitting it makes the next header be read starting from a content byte — which
/// is how this test first reported `UnsupportedVersion(0)`.
fn push_record(out: &mut Vec<u8>, record_type: RecordType, content: &[u8]) {
    let header = Header::new(record_type, 1, content.len() as u16);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(content);
    out.extend(std::iter::repeat_n(0u8, header.padding_length as usize));
}

/// The CGI environment as a list of pairs, so a caller can choose how to split it into records.
fn params_pairs(extra: usize) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut pairs: Vec<(Vec<u8>, Vec<u8>)> = vec![
        (b"REQUEST_METHOD".to_vec(), b"GET".to_vec()),
        (b"REQUEST_URI".to_vec(), b"/items".to_vec()),
        (
            b"SCRIPT_FILENAME".to_vec(),
            b"/srv/app/public/index.php".to_vec(),
        ),
        (b"SERVER_PROTOCOL".to_vec(), b"HTTP/1.1".to_vec()),
        (b"SERVER_SOFTWARE".to_vec(), b"volucer".to_vec()),
        (b"GATEWAY_INTERFACE".to_vec(), b"CGI/1.1".to_vec()),
        (b"REMOTE_ADDR".to_vec(), b"127.0.0.1".to_vec()),
        (b"SERVER_NAME".to_vec(), b"localhost".to_vec()),
        (b"SERVER_PORT".to_vec(), b"8080".to_vec()),
        (b"DOCUMENT_ROOT".to_vec(), b"/srv/app/public".to_vec()),
    ];
    for index in 0..extra {
        pairs.push((
            format!("HTTP_X_HEADER_{index}").into_bytes(),
            format!("value-number-{index}").into_bytes(),
        ));
    }
    pairs
}

/// A FastCGI request as a front-end sends one: BEGIN_REQUEST, then PARAMS, then STDIN.
///
/// `pairs_per_record` splits the environment across several PARAMS records, which is what a
/// front-end does when the environment exceeds one record. It splits at pair boundaries, which
/// is what `volucer-fcgi`'s `Client` does and what nginx's own tests exercise; `0` puts the
/// whole environment in a single record. Whether every client splits at pair boundaries is not
/// something this file asserts — see hazard 38 in `docs/plan.md`.
fn request_bytes(pairs: &[(Vec<u8>, Vec<u8>)], body: &[u8], pairs_per_record: usize) -> Vec<u8> {
    let mut out = Vec::new();

    push_record(
        &mut out,
        RecordType::BeginRequest,
        &BeginRequest::responder(true).encode(),
    );

    let per_record = if pairs_per_record == 0 {
        pairs.len().max(1)
    } else {
        pairs_per_record
    };
    for group in pairs.chunks(per_record.max(1)) {
        let encoded =
            volucer_fcgi::encode_pairs(group.iter().map(|(n, v)| (n.as_slice(), v.as_slice())));
        push_record(&mut out, RecordType::Params, &encoded);
    }
    // The zero-length PARAMS record that terminates the environment.
    push_record(&mut out, RecordType::Params, b"");

    if !body.is_empty() {
        push_record(&mut out, RecordType::Stdin, body);
    }
    push_record(&mut out, RecordType::Stdin, b"");

    out
}

/// Reads one request through the codec as the listener does, through a `BufReader`.
fn buffered_reads(pairs: &[(Vec<u8>, Vec<u8>)], pairs_per_record: usize) -> usize {
    let mut stream = std::io::BufReader::with_capacity(
        READ_BUFFER_LEN,
        Counting {
            input: std::io::Cursor::new(request_bytes(pairs, b"", pairs_per_record)),
            writes: 0,
            out: Vec::new(),
        },
    );
    count_start();
    let next = read_next(&mut stream).expect("a readable request");
    assert!(matches!(
        next,
        Some(volucer_server::frontend::fastcgi::NextRequest::Request(_))
    ));
    READS.with(Cell::get)
}

/// The same request read straight off the transport, which is what the listener did before.
fn unbuffered_reads(pairs: &[(Vec<u8>, Vec<u8>)], pairs_per_record: usize) -> usize {
    let mut transport = Counting {
        input: std::io::Cursor::new(request_bytes(pairs, b"", pairs_per_record)),
        writes: 0,
        out: Vec::new(),
    };
    count_start();
    let next = read_next(&mut transport).expect("a readable request");
    assert!(matches!(
        next,
        Some(volucer_server::frontend::fastcgi::NextRequest::Request(_))
    ));
    READS.with(Cell::get)
}

#[test]
fn reading_one_request_does_not_enter_the_kernel_once_per_field() {
    // A request is BEGIN_REQUEST, then a PARAMS record, its terminator and two STDIN records.
    // Each record is a header followed by a content payload, and `read_record` asks for the two
    // separately, so an unbuffered reader pays about three reads per record. A front-end that
    // splits a large environment across many records — which is exactly what nginx does when the
    // environment outgrows one record — pays that per record, on every request, on a connection
    // it is trying to keep alive.
    let pairs = params_pairs(2);

    let one_record = unbuffered_reads(&pairs, 0);
    let twelve_records = unbuffered_reads(&pairs, 1);

    assert_eq!(one_record, 7, "one record: begin, params, terminator, two stdin");
    // 39, not 40: an unbuffered reader's total depends on how the environment's byte length falls
    // across record boundaries, and `SERVER_SOFTWARE` above is this project's own name. Renaming
    // the project shortened it by two bytes and moved one record boundary, so this total is tied to
    // that fixture and must be re-measured whenever the name in `params_pairs` changes.
    assert_eq!(
        twelve_records, 39,
        "twelve records: {twelve_records} reads for a request that fits in one segment"
    );

    // Read through a buffer, the count tracks how much arrived rather than how it was framed. The
    // whole environment is a few hundred bytes, so both splittings arrive in one fill.
    let buffered_one = buffered_reads(&pairs, 0);
    let buffered_twelve = buffered_reads(&pairs, 1);

    assert_eq!(buffered_one, 1, "the whole request fits in one fill");
    assert_eq!(
        buffered_twelve, 1,
        "splitting the environment into twelve records must not cost twelve more reads"
    );
}

#[test]
fn writing_a_response_does_not_write_a_field_at_a_time() {
    // `write_response` used to write STDOUT's header, its payload, END_REQUEST's header and
    // its payload, then flush — five `write(2)`s on a socket the listener keeps unbuffered
    // for writes (`get_mut()` past the read buffer). On a socket with `TCP_NODELAY` set, which
    // is what a front-end socket wants, every one of those is its own segment and its own
    // syscall. `volucer-fcgi`'s client was fixed for exactly this shape in an earlier pass;
    // this is the same fix on the server's side.
    let stdout = b"Status: 200 OK\r\nContent-Type: text/html\r\n\r\nhello";
    let mut transport = Counting::default();

    count_start();
    write_response(&mut transport, 1, stdout, b"", true).expect("a writable response");

    let writes = WRITES.with(Cell::get);
    assert_eq!(
        writes, 1,
        "a response with no STDERR is one assembled buffer and one write; it was five"
    );

    // The write count alone would not notice a change to the framing, so the bytes are
    // checked too: STDOUT with its content and terminator, then END_REQUEST's 8-byte
    // payload.
    let expected = {
        let mut out = Vec::new();
        push_record(&mut out, RecordType::Stdout, stdout);
        push_record(&mut out, RecordType::Stdout, b"");
        push_record(&mut out, RecordType::EndRequest, &EndRequest::complete().encode());
        out
    };
    assert_eq!(
        transport.out, expected,
        "the assembled response must be byte-identical to the field-by-field one"
    );
}

#[test]
fn a_large_response_body_is_written_in_records_not_in_one_call() {
    // Assembling a response into one buffer must not remove the record framing: a payload
    // past `MAX_CONTENT_LEN` still has to be split, and each stream record still needs its
    // zero-length terminator. The write count can no longer show that — one `write(2)` is one
    // `write(2)` whatever the framing — so this counts **records**, by walking what was
    // written and checking it ends exactly at a record boundary with no bytes left over.
    fn records_for(body_len: usize) -> usize {
        let stdout = vec![b'x'; body_len];
        let mut transport = Counting::default();
        count_start();
        write_response(&mut transport, 1, &stdout, b"", true).expect("a writable response");

        let mut offset = 0;
        let mut records = 0;
        let mut last_header = 0;
        let mut last_type = RecordType::Stdout;
        while offset < transport.out.len() {
            let header = Header::decode(
                transport.out[offset..offset + volucer_fcgi::protocol::HEADER_LEN]
                    .try_into()
                    .expect("a full header"),
            )
            .expect("a decodable header");
            last_header = offset;
            last_type = header.record_type;
            offset += header.total_len();
            records += 1;
        }
        assert_eq!(
            offset,
            transport.out.len(),
            "the framing must end exactly at the end of the buffer, not overrun it"
        );

        // The last record is END_REQUEST's, which is never split. Note that its header and its
        // 8-byte payload are separate records-worth of the framing: the last 8 bytes of the
        // output are END_REQUEST's *status*, not its header, so the header is located by the
        // offset the walk recorded rather than by counting back from the end.
        assert_eq!(last_type, RecordType::EndRequest);
        let end_payload = &transport.out[last_header + volucer_fcgi::protocol::HEADER_LEN..];
        assert_eq!(
            end_payload,
            &EndRequest::complete().encode()[..],
            "END_REQUEST must carry its status, not just its header"
        );
        records
    }

    let small = records_for(12 * 1024);
    let large = records_for(300 * 1024);

    // A 12 KB page is one data record plus its terminator, plus END_REQUEST.
    assert_eq!(small, 3, "12 KB fits in one record, its terminator, and END_REQUEST");
    assert!(
        large > small,
        "a body past MAX_CONTENT_LEN must be split across records, not silently truncated: \
         {large} records for 300 KB against {small} for 12 KB"
    );

    // Splitting into records no longer costs a syscall each: the whole framed response is
    // assembled first, so even 300 KB leaves in one `write(2)`. That is the point of the
    // change — `write_records` flushed after every record, which made a large page many
    // small segments on a socket with `TCP_NODELAY` set.
    let mut big = Counting::default();
    count_start();
    write_response(&mut big, 1, &vec![b'x'; 300 * 1024], b"", true)
        .expect("a writable response");
    assert_eq!(
        WRITES.with(Cell::get),
        1,
        "{large} records must leave in one write, not one write per record"
    );
}

#[test]
fn the_backend_response_is_relayed_without_re_parsing_it() {
    // Pinned so the relay stays a copy: `write_response` takes the backend's `stdout` verbatim,
    // which is why the response costs a fixed number of *records* rather than one per header.
    // The headers are never re-serialised, so the STDOUT payload in the output is byte-equal to
    // what the backend sent.
    let stdout = b"Status: 200 OK\r\nX-A: 1\r\nX-B: 2\r\nX-C: 3\r\n\r\nbody";
    let mut transport = Counting::default();

    count_start();
    write_response(&mut transport, 1, stdout, b"", true).expect("a writable response");

    assert_eq!(
        WRITES.with(Cell::get),
        1,
        "three headers must not add three writes; the block is relayed whole"
    );

    // Skip the 8-byte record header: what follows is the backend's bytes, unchanged.
    assert_eq!(
        &transport.out[volucer_fcgi::protocol::HEADER_LEN..volucer_fcgi::protocol::HEADER_LEN + stdout.len()],
        stdout,
        "the backend's payload must reach the wire verbatim"
    );
}

#[test]
fn an_end_request_record_is_always_terminated() {
    // The `EndRequest` terminator is what closes a request for the client; a missing one leaves
    // nginx waiting. This is the invariant, expressed as bytes rather than as a write count.
    let end = EndRequest::complete();
    let mut out = Vec::new();
    let header = Header::new(RecordType::EndRequest, 1, end.encode().len() as u16);
    out.extend_from_slice(&header.encode());
    out.extend_from_slice(&end.encode());

    // An eight-byte header and an eight-byte payload, with no padding because eight is already
    // a multiple of eight.
    assert_eq!(header.padding_length, 0);
    assert_eq!(out.len(), 16);
    assert_eq!(end.app_status, 0);
    assert_eq!(
        end.protocol_status,
        volucer_fcgi::ProtocolStatus::RequestComplete
    );
}
