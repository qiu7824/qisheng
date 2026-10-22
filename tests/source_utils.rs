//! Compatibility checks against the native utility bridge, with original data.
use boa_engine::{Context, Source};
use lx_gpui_player::source_utils;

fn context() -> Context {
    let mut context = Context::default();
    source_utils::register(&mut context).expect("register utility bridge");
    context.eval(Source::from_bytes(r#"
        const B = __lxHostUtils.Buffer;
        const u = __lxHostUtils.utils;
        function expect(value, label) { if (!value) throw new Error(label); }
        function throws(fn, label) { let caught = false; try { fn(); } catch (_) { caught = true; } expect(caught, label); }
    "#)).expect("test helpers");
    context
}

fn evaluate(context: &mut Context, script: &str) {
    context
        .eval(Source::from_bytes(script))
        .unwrap_or_else(|error| panic!("utility behavior: {error}"));
}

#[test]
fn buffer_encodings_and_copy_vs_shared_views() {
    evaluate(
        &mut context(),
        r#"
        expect(B.from('é水😀').toString() === 'é水😀', 'UTF8 roundtrip');
        expect(B.from('\ud800a\udc00').toString('hex') === 'efbfbd61efbfbd', 'UTF8 lone surrogate replacement');
        expect(B.from('é', 'ascii').toString('hex') === 'e9', 'ASCII encoding keeps low8bits');
        expect(B.from([0xe9]).toString('ascii') === 'i', 'ASCII decoding masks high bit');
        expect(B.from('😀', 'latin1').toString('hex') === '3d00', 'latin1 uses UTF16 code units');
        expect(B.from('\ud800', 'utf16le').toString('utf16le').charCodeAt(0) === 0xd800, 'UTF16LE preserves lone surrogate');
        expect(B.from('A水', 'UCS-2').toString('HEX') === '4100346c', 'case insensitive aliases');
        expect(B.from([65, 0, 99]).toString('utf16le') === 'A', 'UTF16 drops incomplete unit');
        expect(B.from('1ag123', 'hex').toString('hex') === '1a', 'hex invalid-pair truncation');
        expect(B.from('1a7', 'hex').toString('hex') === '1a', 'hex trailing nibble truncation');
        expect(B.from(' YW Jj\n!!', 'base64').toString() === 'abc', 'base64 whitespace and tolerance');
        expect(B.from('-_8', 'base64').toString('base64url') === '-_8', 'base64url without padding');
        expect(u.buffer.bufToString('é', 'hex') === 'e9', 'bufToString string input is binary');
        expect(B.from([-1, 257, NaN, 1.9]).toString('hex') === 'ff010001', 'array byte coercion');
        const input = new Uint8Array([1, 2, 3, 4]);
        const copied = B.from(input), shared = B.from(input.buffer, 1, 2);
        input[1] = 9;
        expect(copied[1] === 2 && shared[0] === 9, 'typed array copies, ArrayBuffer shares');
        shared[1] = 7; expect(input[2] === 7, 'shared writes reach backing store');
        expect(B.from(new Uint16Array([0x1234])).toString('hex') === '34', 'typed array elements truncate');
        expect(B.from(new Uint16Array([0x1234, 0x1ff])).toString('hex') === '34ff', 'one low byte per wider typed array element');
        expect(B.from(new DataView(input.buffer)).length === 0, 'DataView has no array elements');
        expect(B.from({type:'Buffer', data:[3,4]}).toString('hex') === '0304', 'Buffer JSON format');
        throws(() => B.from(5), 'from number rejected');
        throws(() => B.alloc(2097153), 'buffer allocation cap');
        throws(() => B.from('x', 'unknown'), 'unknown encoding rejected');
        expect(typeof __lx_native_utils === 'undefined', 'native bridge hidden');
        expect(Object.isFrozen(u) && Object.isFrozen(u.buffer), 'utility facade frozen');
    "#,
    );
}

#[test]
fn common_buffer_methods_and_integers() {
    evaluate(
        &mut context(),
        r#"
        const bytes = B.from([1,2,3,4]);
        const sliced = bytes.slice(1,3); sliced[0] = 9;
        expect(bytes[1] === 9, 'Node slice shares memory');
        expect(B.isBuffer(bytes.map(n => n + 1)), 'map returns Buffer');
        expect(bytes.map(n => n + 1).toString('hex') === '020a0405', 'Buffer map bytes');
        expect(B.concat([B.from('a'), B.from('bc')], 5).toString('hex') === '6162630000', 'concat zero pads explicit longer length');
        expect(B.concat([B.from('abc')], 2).toString() === 'ab', 'concat truncates');
        expect(B.alloc(5, 'ab').toString() === 'ababa', 'fill repeats encoded pattern');
        expect(B.alloc(3, 0xff).toString('hex') === 'ffffff', 'numeric fill');
        expect(B.from('abc').equals(B.from('abc')), 'equals');
        expect(B.compare(B.from('ab'), B.from('abc')) === -1, 'compare length');
        const overlap = B.from('abcd'); overlap.copy(overlap, 1, 0, 3);
        expect(overlap.toString() === 'aabc', 'overlap copy has memmove semantics');
        expect(B.from('abc').toString('utf8', 1, 2) === 'b', 'toString range');
        const numbers = B.alloc(12);
        expect(numbers.writeUInt32LE(0x12345678, 0) === 4 && numbers.readUInt32LE(0) === 0x12345678, 'UInt32LE');
        numbers.writeUInt16BE(0xabcd, 4); expect(numbers.readUInt16BE(4) === 0xabcd, 'UInt16BE');
        numbers.writeInt32BE(-2147483648, 6); expect(numbers.readInt32BE(6) === -2147483648, 'Int32BE');
        numbers.writeInt8(-5, 10); expect(numbers.readInt8(10) === -5, 'Int8');
        expect(B.byteLength('水') === 3 && B.byteLength('水','utf16le') === 2, 'byteLength');
        expect(B.byteLength('1ag123','hex') === 3, 'hex byteLength predicts capacity without parsing');
        const written = B.alloc(5);
        expect(written.write('abcé',0,4,'utf8') === 3 && written.toString('hex') === '6162630000', 'write avoids partial UTF8');
        expect(B.alloc(3).write('ab','utf16le') === 2, 'write avoids partial UTF16');
        throws(() => numbers.readUInt32LE(10), 'read bound');
        throws(() => numbers.writeUInt8(256), 'integer range');
        throws(() => B.alloc(4).fill(B.alloc(0)), 'empty fill pattern');
    "#,
    );
}

#[test]
fn crypto_arguments_real_vectors_and_random_lengths() {
    evaluate(
        &mut context(),
        r#"
        expect(u.crypto.md5('abc') === '900150983cd24fb0d6963f7d28e17f72', 'MD5 RFC1321 abc');
        expect(u.crypto.md5(B.from('abc')) === u.crypto.md5('abc'), 'MD5 Buffer');
        expect(u.crypto.md5(new DataView(B.from('abc').buffer)) === u.crypto.md5('abc'), 'MD5 DataView bytes');
        throws(() => u.crypto.md5([97,98,99]), 'Node hash rejects plain arrays');
        const key = B.alloc(16), iv = B.alloc(16);
        expect(u.crypto.aesEncrypt('', 'aes-128-ecb', key, null).toString('hex') === '0143db63ee66b0cdff9f69917680151e', 'empty PKCS7 ECB known vector');
        expect(u.crypto.aesEncrypt('', 'aes-128-cbc', key, iv).toString('hex') === '0143db63ee66b0cdff9f69917680151e', 'empty PKCS7 CBC zero IV');
        expect(u.crypto.aesEncrypt('hello', 'aes-128-cbc', '0123456789abcdef', '0123456789abcdef').length === 16, 'key/IV strings UTF8');
        expect(u.crypto.randomBytes(0).length === 0, 'zero random bytes');
        expect(B.isBuffer(u.crypto.randomBytes(32)) && u.crypto.randomBytes(32).length === 32, 'random returns Buffer');
        throws(() => u.crypto.randomBytes(1.5), 'random integral size');
        throws(() => u.crypto.randomBytes(2097153), 'random cap');
        throws(() => u.crypto.aesEncrypt('x','aes-128-cbc',key,null), 'CBC IV required');
        throws(() => u.crypto.aesEncrypt('x','aes-128-gcm',key,iv), 'unimplemented AES mode');
        throws(() => u.crypto.rsaEncrypt(B.from('x'), 'key'), 'RSA explicit unsupported');
    "#,
    );
}

#[test]
fn zlib_returns_promises_buffers_and_rejections() {
    let mut context = context();
    evaluate(
        &mut context,
        r#"
        globalThis.utilityResult = 'pending';
        const input = B.from('Original legal fixture\0水');
        const promise = u.zlib.deflate(input);
        expect(typeof promise.then === 'function', 'deflate returns Promise');
        promise.then(compressed => {
          expect(B.isBuffer(compressed), 'deflate returns Buffer');
          return u.zlib.inflate(compressed);
        }).then(expanded => {
          expect(expanded.equals(input), 'zlib roundtrip');
          return u.zlib.inflate(B.from('invalid'));
        }).then(() => { utilityResult = 'invalid-stream-accepted'; }, error => {
          expect(error instanceof Error, 'invalid zlib rejects Error');
          utilityResult = 'passed';
        });
    "#,
    );
    context.run_jobs().expect("run zlib jobs");
    evaluate(
        &mut context,
        "expect(utilityResult === 'passed', 'zlib Promise completion');",
    );
}
