//! Original fixtures checked against Node's Buffer and LX's RSA_NO_PADDING helper.
use boa_engine::{Context, Source};
use lx_gpui_player::source_utils;
use serde_json::{Value, json};
use std::{
    io::{BufRead, BufReader, Write},
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

const SPKI: &str = "-----BEGIN PUBLIC KEY-----\nMIGfMA0GCSqGSIb3DQEBAQUAA4GNADCBiQKBgQCjsVKjMII4NNbTy1VzUW7+/wBh\nl4j7iXb3HJ9QVm3RFblnCggE0FonktXlkc6cVcRC+Tkb0Y3KCks44Qp/y56Lzwi0\nW7vx0dJ8ZySFJl047JcvB7Hl7eGcb21qJC4O5/I42ztWDkv0v86jcHYKR7VpHAyB\nmPdRcUmzfExF+3q+NwIDAQAB\n-----END PUBLIC KEY-----\n";
const PKCS1: &str = "-----BEGIN RSA PUBLIC KEY-----\nMIGJAoGBAKOxUqMwgjg01tPLVXNRbv7/AGGXiPuJdvccn1BWbdEVuWcKCATQWieS\n1eWRzpxVxEL5ORvRjcoKSzjhCn/LnovPCLRbu/HR0nxnJIUmXTjsly8HseXt4Zxv\nbWokLg7n8jjbO1YOS/S/zqNwdgpHtWkcDIGY91FxSbN8TEX7er43AgMBAAE=\n-----END RSA PUBLIC KEY-----\n";
const RSA_ABC: &str = "3137a3598dd244925b260739a0059d6e3f58806226ab0b11b771578cfa596167fea9fd9a70b59c69af33437ae11439e3885fa780bf7bc8e5d6b5091da701d8d3174473325b3221a2925ab7bc362abd6c46b797c34ca9ca6e84a1a89656841d52e2d2fea4669440c58787b4ccb7f2a2b948faa1fc6ca0b828a4949c60f377a5d1";
const RSA_127: &str = "58ba09869f8881aa0c926f122e367bbcadd8216c338dafffef6ddedf4a0d4a9c5c22fdb69ab193d951e9cd02e1213f3b7e4ecaa09042fb1972ef551506767f65ea1655a6488d06779e1c7926d4efd632e1b35f00e69c1f6e0566b4828fb795af4c4452a1825bd643fe9ea733d02a270ddef40a7f26f2fa7dd360f1dd8a3e71a6";
const PRIVATE_PKCS1: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIICXgIBAAKBgQCjsVKjMII4NNbTy1VzUW7+/wBhl4j7iXb3HJ9QVm3RFblnCggE
0FonktXlkc6cVcRC+Tkb0Y3KCks44Qp/y56Lzwi0W7vx0dJ8ZySFJl047JcvB7Hl
7eGcb21qJC4O5/I42ztWDkv0v86jcHYKR7VpHAyBmPdRcUmzfExF+3q+NwIDAQAB
AoGBAJXq81pONRUxH4d99FbXQCb+eOTomiZ1caVEa4xye8VAdjsubsWp5MfVrVzB
u05nJoaIJouZ0pg18Uq4oJsGa778dzCbJ7iVM/sxMf9YdHFWt+JPScjk2gmUgFkd
aYxF6OKzSg8j7Ef9v4y+wxXVM34Viifht0Xpv+EKtd2OEMAhAkEA15uhD80wha4T
lhhq31gUZj2tfEzOht2X1D+tam3tBOtQdjQjZVRzPlwTRRtkWHYHsRxxITS7rQJC
nV/sw/EgHwJBAMJb4ee+dUoTVI1lzdG7DNk5UTLm8ipT7VeNLu3TGKsBEg4v+G7T
0NyuFbeIHJR41aMq98X+hRPIDlPM3MUjPukCQD06SePF3lioFZHTP05Logdizoh0
+1VbSkmRB7soml2CQag/qCmXcN6toR9H5xgj9I08SXgnlIBOGwK8Ae2m5PsCQQC2
/2BCnRc+OdkgkTwrPx0n4knLzZsxs3gDOVF4Z4wQZp+jcBNP9mvvfJHCDowqV2N/
kFNevvh52aFz0a4Eh90RAkEAjEpb0xHT0ZLQchnhGOGQPb82kAz3rID15WOS2bgg
3iialbmpBRkaKAnsqvobnMad3gMWZRPKzhaTfIc1AfOgYQ==
-----END RSA PRIVATE KEY-----
"#;
const PRIVATE_PKCS8: &str = r#"-----BEGIN PRIVATE KEY-----
MIICeAIBADANBgkqhkiG9w0BAQEFAASCAmIwggJeAgEAAoGBAKOxUqMwgjg01tPL
VXNRbv7/AGGXiPuJdvccn1BWbdEVuWcKCATQWieS1eWRzpxVxEL5ORvRjcoKSzjh
Cn/LnovPCLRbu/HR0nxnJIUmXTjsly8HseXt4ZxvbWokLg7n8jjbO1YOS/S/zqNw
dgpHtWkcDIGY91FxSbN8TEX7er43AgMBAAECgYEAlerzWk41FTEfh330VtdAJv54
5OiaJnVxpURrjHJ7xUB2Oy5uxankx9WtXMG7Tmcmhogmi5nSmDXxSrigmwZrvvx3
MJsnuJUz+zEx/1h0cVa34k9JyOTaCZSAWR1pjEXo4rNKDyPsR/2/jL7DFdUzfhWK
J+G3Rem/4Qq13Y4QwCECQQDXm6EPzTCFrhOWGGrfWBRmPa18TM6G3ZfUP61qbe0E
61B2NCNlVHM+XBNFG2RYdgexHHEhNLutAkKdX+zD8SAfAkEAwlvh5751ShNUjWXN
0bsM2TlRMubyKlPtV40u7dMYqwESDi/4btPQ3K4Vt4gclHjVoyr3xf6FE8gOU8zc
xSM+6QJAPTpJ48XeWKgVkdM/TkuiB2LOiHT7VVtKSZEHuyiaXYJBqD+oKZdw3q2h
H0fnGCP0jTxJeCeUgE4bArwB7abk+wJBALb/YEKdFz452SCRPCs/HSfiScvNmzGz
eAM5UXhnjBBmn6NwE0/2a+98kcIOjCpXY3+QU16++HnZoXPRrgSH3RECQQCMSlvT
EdPRktByGeEY4ZA9vzaQDPesgPXlY5LZuCDeKJqVuakFGRooCeyq+hucxp3eAxZl
E8rOFpN8hzUB86Bh
-----END PRIVATE KEY-----
"#;
fn context() -> Context {
    let mut c = Context::default();
    source_utils::register(&mut c).unwrap();
    eval(
        &mut c,
        r#"const B=__lxHostUtils.Buffer,u=__lxHostUtils.utils;function expect(value,label){if(!value)throw Error(label);}function error(fn,name,code,message){let e;try{fn();}catch(caught){e=caught;}expect(e&&e.name===name,'error type '+name);expect(e.code===code,'error code '+code);if(message!==undefined)expect(e.message===message,'error message '+e.message);} "#,
    );
    c
}
fn eval(c: &mut Context, script: &str) {
    c.eval(Source::from_bytes(script))
        .unwrap_or_else(|e| panic!("utility fixture failed: {e}"));
}
fn key(c: &mut Context, name: &str, pem: &str) {
    eval(
        c,
        &format!("globalThis.{name}={};", serde_json::to_string(pem).unwrap()),
    );
}

#[test]
fn float_and_double_ieee754_endian_views_special_values_and_coercions() {
    eval(
        &mut context(),
        r#"
      const b=B.alloc(16);expect(b.writeFloatLE(1.5)===4,'FloatLE offset');expect(b.writeFloatBE(-2.25,4)===8,'FloatBE offset');expect(b.writeDoubleLE(Math.PI,8)===16,'DoubleLE offset');
      expect(b.toString('hex')==='0000c03fc0100000182d4454fb210940','IEEE754 bytes');expect(b.readFloatLE()===1.5&&b.readFloatBE(4)===-2.25&&b.readDoubleLE(8)===Math.PI,'IEEE754 values');
      const view=B.alloc(20).subarray(2,18);view.writeDoubleBE(-0,4);expect(view.toString('hex')==='00000000800000000000000000000000','view byteOffset');expect(Object.is(view.readDoubleBE(4),-0),'signed zero');
      const special=B.alloc(16);special.writeFloatLE(NaN);special.writeFloatBE(Infinity,4);special.writeDoubleLE(-Infinity,8);expect(special.toString('hex')==='0000c07f7f800000000000000000f0ff','NaN/infinity encoding');expect(Number.isNaN(special.readFloatLE()),'NaN read');
      expect(B.alloc(4).writeFloatLE('1.5')===4,'float string coercion');expect(B.from('0000c03f','hex').readFloatLE()===1.5,'float from wire');
    "#,
    );
}

#[test]
fn bigint_signed_unsigned_endian_limits_and_node_aliases() {
    eval(
        &mut context(),
        r#"
      const b=B.alloc(32);expect(b.writeBigUInt64LE((1n<<64n)-1n)===8,'unsigned return');b.writeBigUInt64BE(0x0123456789abcdefn,8);b.writeBigInt64LE(-(1n<<63n),16);b.writeBigInt64BE(-1n,24);
      expect(b.toString('hex')==='ffffffffffffffff0123456789abcdef0000000000000080ffffffffffffffff','BigInt64 bytes');expect(b.readBigUInt64LE()===(1n<<64n)-1n&&b.readBigUInt64BE(8)===0x0123456789abcdefn,'unsigned reads');expect(b.readBigInt64LE(16)===-(1n<<63n)&&b.readBigInt64BE(24)===-1n,'signed reads');
      const alias=B.alloc(8);alias.writeBigUint64LE(17n);expect(alias.readBigUint64LE()===17n,'BigUint spelling');alias.writeUint32BE(0xabcdef);expect(alias.readUint32BE()===0xabcdef,'Uint spelling');
      const unaligned=B.alloc(10).subarray(1,9);unaligned.writeBigInt64BE(-7n);expect(unaligned.readBigInt64BE()===-7n,'BigInt view offset');
    "#,
    );
}

#[test]
fn node_numeric_offset_type_range_and_overflow_errors_are_preserved() {
    eval(
        &mut context(),
        r#"
      error(()=>B.alloc(8).readFloatLE('0'),'TypeError','ERR_INVALID_ARG_TYPE','The "offset" argument must be of type number. Received type string (\'0\')');
      error(()=>B.alloc(8).readDoubleBE(null),'TypeError','ERR_INVALID_ARG_TYPE','The "offset" argument must be of type number. Received null');
      error(()=>B.alloc(8).readDoubleBE(true),'TypeError','ERR_INVALID_ARG_TYPE','The "offset" argument must be of type number. Received type boolean (true)');
      error(()=>B.alloc(8).readDoubleBE(0n),'TypeError','ERR_INVALID_ARG_TYPE','The "offset" argument must be of type number. Received type bigint (0n)');
      error(()=>B.alloc(8).readDoubleBE(Symbol('x')),'TypeError','ERR_INVALID_ARG_TYPE','The "offset" argument must be of type number. Received type symbol (Symbol(x))');
      error(()=>B.alloc(8).writeDoubleBE(1n),'TypeError',undefined,'Cannot convert a BigInt value to a number');error(()=>B.alloc(4).writeUInt32LE(Symbol('x')),'TypeError',undefined,'Cannot convert a Symbol value to a number');
      error(()=>B.alloc(8).readFloatLE(.5),'RangeError','ERR_OUT_OF_RANGE','The value of "offset" is out of range. It must be an integer. Received 0.5');
      error(()=>B.alloc(8).readFloatLE(NaN),'RangeError','ERR_OUT_OF_RANGE','The value of "offset" is out of range. It must be an integer. Received NaN');
      error(()=>B.alloc(8).readFloatLE(5),'RangeError','ERR_OUT_OF_RANGE','The value of "offset" is out of range. It must be >= 0 and <= 4. Received 5');
      error(()=>B.alloc(1).readDoubleLE(),'RangeError','ERR_BUFFER_OUT_OF_BOUNDS','Attempt to access memory outside buffer bounds');
      error(()=>B.alloc(8).writeBigUInt64LE(1),'TypeError',undefined,'Cannot mix BigInt and other types, use explicit conversions');
      error(()=>B.alloc(8).writeBigUInt64LE(1n<<64n),'RangeError','ERR_OUT_OF_RANGE','The value of "value" is out of range. It must be >= 0n and < 2n ** 64n. Received 18_446_744_073_709_551_616n');
      error(()=>B.alloc(8).writeBigUInt64LE(-1n),'RangeError','ERR_OUT_OF_RANGE');error(()=>B.alloc(8).writeBigInt64BE(1n<<63n),'RangeError','ERR_OUT_OF_RANGE');
      error(()=>B.alloc(8).writeBigUInt64LE(1,-1),'RangeError','ERR_OUT_OF_RANGE');
      error(()=>B.alloc(6).readUIntLE(undefined,2),'TypeError','ERR_INVALID_ARG_TYPE');error(()=>B.alloc(6).readUIntLE(0,'2'),'TypeError','ERR_INVALID_ARG_TYPE');
      error(()=>B.alloc(1).writeUInt8(255.5),'RangeError','ERR_OUT_OF_RANGE');error(()=>B.alloc(1).writeUInt16LE(999999,-1),'RangeError','ERR_OUT_OF_RANGE','The value of "value" is out of range. It must be >= 0 and <= 65535. Received 999999');
      const trunc=B.alloc(6);trunc.writeInt16LE('-1.5');trunc.writeInt16BE(NaN,2);trunc.writeUInt16LE(undefined,4);expect(trunc.toString('hex')==='ffff00000000','integer coercion/truncation');
    "#,
    );
}

#[test]
fn buffer_json_encoding_write_and_copy_boundaries_match_node() {
    eval(
        &mut context(),
        r#"
      const original=B.from([0,128,255]);expect(JSON.stringify(original)==='{"type":"Buffer","data":[0,128,255]}','toJSON');expect(B.from(JSON.parse(JSON.stringify(original))).equals(original),'JSON input roundtrip');
      expect(B.alloc(0).toString('bad')===''&&B.from('ab').toString('bad',9)==='','empty range avoids decoding');
      error(()=>B.from('abc').toString(''),'TypeError','ERR_UNKNOWN_ENCODING','Unknown encoding: ');error(()=>B.from('abc').toString(5),'TypeError','ERR_UNKNOWN_ENCODING','Unknown encoding: 5');error(()=>B.from('abc').toString(null),'TypeError','ERR_UNKNOWN_ENCODING','Unknown encoding: null');
      const utf8=B.alloc(4);expect(utf8.write('😀a',0,3)===0&&utf8.toString('hex')==='00000000','UTF8 boundary');expect(B.alloc(3).write('ab','utf16le')===2,'UTF16 boundary');
      error(()=>B.alloc(2).write('x',0,3),'RangeError','ERR_OUT_OF_RANGE');expect(B.alloc(2).write('abc',1,2)===1,'length within buffer but beyond remainder');
      const overlap=B.from('abcde');expect(overlap.copy(overlap.subarray(1),0,0,4)===4&&overlap.toString()==='aabcd','overlap shared views');
      const infinity=B.alloc(2);expect(B.from('ab').copy(infinity,Infinity)===2&&infinity.toString()==='ab','copy nonfinite index');
      const fractional=B.alloc(5);expect(B.from('abcd').copy(fractional,1.9,1.9,3.9)===2&&fractional.toString('hex')==='0062630000','copy floor indices');
      error(()=>B.from('ab').copy(B.alloc(2),0,-.5),'RangeError','ERR_OUT_OF_RANGE','The value of "sourceStart" is out of range. It must be >= 0 && <= 2. Received -1');expect(B.from('ab').copy(B.alloc(0),0,0,2)===0,'empty destination');expect(B.from('ab').copy(B.alloc(2),10)===0,'past destination');
      expect(B.from(' YW Jj\n!!','base64').toString()==='abc','base64 tolerance');expect(B.from('1ag123','hex').toString('hex')==='1a','hex truncation');expect(B.from('\ud800','utf16le').toString('utf16le').charCodeAt(0)===0xd800,'UTF16 surrogate retained');
    "#,
    );
}

#[test]
fn active_lx_rsa_helper_matches_node_no_padding_pem_and_block_goldens() {
    let mut c = context();
    key(&mut c, "spki", SPKI);
    key(&mut c, "pkcs1", PKCS1);
    key(&mut c, "private1", PRIVATE_PKCS1);
    key(&mut c, "private8", PRIVATE_PKCS8);
    key(&mut c, "golden", RSA_ABC);
    key(&mut c, "golden127", RSA_127);
    eval(
        &mut c,
        r#"
      const encrypted=u.crypto.rsaEncrypt(B.from('abc'),spki);expect(B.isBuffer(encrypted)&&encrypted.length===128&&encrypted.toString('hex')===golden,'SPKI RSA golden');expect(u.crypto.rsaEncrypt(new Uint8Array([97,98,99]),B.from(pkcs1)).toString('hex')===golden,'PKCS1 and Uint8Array');
      expect(u.crypto.rsaEncrypt(B.from('abc'),private1).equals(encrypted)&&u.crypto.rsaEncrypt(B.from('abc'),private8).equals(encrypted),'private PEM public components');
      expect(u.crypto.rsaEncrypt(B.alloc(0),spki).toString('hex')==='00'.repeat(128),'zero block');expect(u.crypto.rsaEncrypt(B.alloc(127,255),spki).toString('hex')===golden127,'left padding');expect(u.crypto.rsaEncrypt(B.concat([B.alloc(125),B.from('abc')]),spki).equals(encrypted),'full block');
      error(()=>u.crypto.rsaEncrypt(B.alloc(129),spki),'RangeError','ERR_OUT_OF_RANGE');error(()=>u.crypto.rsaEncrypt('abc',spki),'TypeError','ERR_INVALID_ARG_TYPE');
      error(()=>u.crypto.rsaEncrypt(B.alloc(128,255),spki),'Error','ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS','error:02000084:rsa routines::data too large for modulus');
      const modulus=B.from('a3b152a330823834d6d3cb5573516efeff00619788fb8976f71c9f50566dd115b9670a0804d05a2792d5e591ce9c55c442f9391bd18dca0a4b38e10a7fcb9e8bcf08b45bbbf1d1d27c672485265d38ec972f07b1e5ede19c6f6d6a242e0ee7f238db3b560e4bf4bfcea370760a47b5691c0c8198f7517149b37c4c45fb7abe37','hex');
      error(()=>u.crypto.rsaEncrypt(modulus,spki),'Error','ERR_OSSL_RSA_DATA_TOO_LARGE_FOR_MODULUS');
      const justBelow=B.from(modulus);justBelow[127]--;expect(u.crypto.rsaEncrypt(justBelow,spki).equals(justBelow),'modulus minus one accepted');
      let invalid=false;try{u.crypto.rsaEncrypt(modulus,spki);}catch(e){invalid=e.library==='rsa routines'&&e.reason==='data too large for modulus';}expect(invalid,'OpenSSL RSA error fields');
      invalid=false;try{u.crypto.rsaEncrypt(B.from('abc'),'not a key');}catch(e){invalid=e instanceof Error;}expect(invalid,'invalid PEM');
      expect(u.crypto.md5('abc')==='900150983cd24fb0d6963f7d28e17f72','MD5 regression');expect(u.crypto.aesEncrypt('','aes-128-ecb',B.alloc(16),null).toString('hex')==='0143db63ee66b0cdff9f69917680151e','AES regression');
    "#,
    );
}

#[test]
fn complete_utils_execute_inside_the_real_process_worker() {
    let script = format!(
        r#"/* @name Utility worker fixture */
      const key={key};const golden={golden};
      lx.on(lx.EVENT_NAMES.request,async()=>{{
        const B=lx.utils.buffer.from([0]).constructor,b=B.alloc(16);b.writeDoubleBE(Math.PI);b.writeBigInt64LE(-9n,8);
        if(b.readDoubleBE()!==Math.PI||b.readBigInt64LE(8)!==-9n)throw Error('numeric bridge');
        if(JSON.stringify(B.from([0,255]))!=='{{"type":"Buffer","data":[0,255]}}')throw Error('Buffer JSON');
        if(lx.utils.crypto.rsaEncrypt(B.from('abc'),key).toString('hex')!==golden)throw Error('RSA bridge');
        const packed=await lx.utils.zlib.deflate(B.from('worker compression'));if((await lx.utils.zlib.inflate(packed)).toString()!=='worker compression')throw Error('zlib bridge');
        return 'http://127.0.0.1:9/utils.wav';
      }});lx.send(lx.EVENT_NAMES.inited,{{sources:{{kw:{{type:'music',actions:['musicUrl'],qualitys:['128k']}}}}}});
    "#,
        key = serde_json::to_string(SPKI).unwrap(),
        golden = serde_json::to_string(RSA_ABC).unwrap()
    );
    let mut command = Command::new(env!("CARGO_BIN_EXE_lx-native-cli"));
    command
        .arg("--source-worker")
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", root);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    let mut child = command.spawn().unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            let Ok(line) = line else { break };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                break;
            };
            if sender.send(value).is_err() {
                break;
            }
        }
    });
    let mut exchange = |value: Value| {
        let mut bytes = serde_json::to_vec(&value).unwrap();
        bytes.push(b'\n');
        stdin.write_all(&bytes).unwrap();
        stdin.flush().unwrap();
        receiver
            .recv_timeout(Duration::from_secs(12))
            .expect("real utility worker response")
    };
    let init = exchange(json!({"op":"init","script":script}));
    let result = exchange(
        json!({"op":"request","request":{"source":"kw","action":"musicUrl","info":{"type":"128k","musicInfo":{"source":"kw","name":"Fixture","songmid":"fixture","singer":"Test","interval":"00:01","albumName":"","img":"","albumId":"","types":[],"_types":{},"typeUrl":{}}}}}),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(init["kind"], "initialized", "{init}");
    assert_eq!(result["url"], "http://127.0.0.1:9/utils.wav", "{result}");
}
