//! Buffer surface fixtures recorded independently with Node 24.19.
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
fn context() -> Context {
    let mut c = Context::default();
    source_utils::register(&mut c).unwrap();
    eval(
        &mut c,
        r#"const B=__lxHostUtils.Buffer;function expect(value,label){if(!value)throw Error(label);}function throws(fn,name,code){let caught;try{fn();}catch(e){caught=e;}expect(caught&&caught.name===name,'error type');if(code!==undefined)expect(caught.code===code,'error code');}"#,
    );
    c
}
fn eval(c: &mut Context, script: &str) {
    c.eval(Source::from_bytes(script))
        .unwrap_or_else(|e| panic!("Buffer surface fixture: {e}"));
}

#[test]
fn node_buffer_own_surface_names_constructor_and_symbols() {
    eval(
        &mut context(),
        r#"
      const expected=['constructor','readBigUInt64LE','readBigUInt64BE','readBigUint64LE','readBigUint64BE','readBigInt64LE','readBigInt64BE','writeBigUInt64LE','writeBigUInt64BE','writeBigUint64LE','writeBigUint64BE','writeBigInt64LE','writeBigInt64BE','readUIntLE','readUInt32LE','readUInt16LE','readUInt8','readUIntBE','readUInt32BE','readUInt16BE','readUintLE','readUint32LE','readUint16LE','readUint8','readUintBE','readUint32BE','readUint16BE','readIntLE','readInt32LE','readInt16LE','readInt8','readIntBE','readInt32BE','readInt16BE','writeUIntLE','writeUInt32LE','writeUInt16LE','writeUInt8','writeUIntBE','writeUInt32BE','writeUInt16BE','writeUintLE','writeUint32LE','writeUint16LE','writeUint8','writeUintBE','writeUint32BE','writeUint16BE','writeIntLE','writeInt32LE','writeInt16LE','writeInt8','writeIntBE','writeInt32BE','writeInt16BE','readFloatLE','readFloatBE','readDoubleLE','readDoubleBE','writeFloatLE','writeFloatBE','writeDoubleLE','writeDoubleBE','asciiSlice','base64Slice','base64urlSlice','latin1Slice','hexSlice','ucs2Slice','utf8Slice','asciiWrite','base64Write','base64urlWrite','latin1Write','hexWrite','ucs2Write','utf8Write','parent','offset','copy','toString','equals','inspect','compare','indexOf','lastIndexOf','includes','fill','write','toJSON','subarray','slice','swap16','swap32','swap64','toLocaleString'];
      expect(JSON.stringify(Object.getOwnPropertyNames(B.prototype).sort())===JSON.stringify(expected.sort()),'Node prototype inventory');
      const statics=['length','name','prototype','poolSize','from','copyBytesFrom','of','alloc','allocUnsafe','allocUnsafeSlow','isBuffer','compare','isEncoding','concat','byteLength'];expect(JSON.stringify(Object.getOwnPropertyNames(B).sort())===JSON.stringify(statics.sort()),'Node static inventory');
      expect(B.name==='Buffer'&&B.length===3&&B.alloc.length===3&&B.prototype.compare.length===5,'function metadata');expect(B[Symbol.species].name==='FastBuffer','typed species');expect(typeof B.prototype[Symbol.for('nodejs.util.inspect.custom')]==='function','inspect symbol');
      expect(B('abc').toString()==='abc'&&new B('abc').toString()==='abc'&&new B([1,257]).toString('hex')==='0101','callable/new constructor');expect(B.alloc(1.5).length===1,'allocation truncation');
      const view=B.alloc(4).subarray(1,3);expect(view.parent===view.buffer&&view.offset===view.byteOffset,'parent and offset aliases');expect(B.of(1,257).toString('hex')==='0101','of');expect(B.allocUnsafeSlow(3).toString('hex')==='000000','unsafe host zeroing');
      throws(()=>B.alloc(2097153),'RangeError');expect(B.from({length:NaN}).length===0&&B.from({length:'2',0:1}).length===0,'array-like length conversion');
      expect(B.concat([],1.5).length===0&&B.from(new ArrayBuffer(2),0,-1).length===0,'empty and nonnegative length coercion');throws(()=>B.from(new ArrayBuffer(2),3),'RangeError','ERR_BUFFER_OUT_OF_BOUNDS');throws(()=>B.from(new ArrayBuffer(2),0,3),'RangeError','ERR_BUFFER_OUT_OF_BOUNDS');
    "#,
    );
}

#[test]
fn search_strings_numbers_views_multi_encoding_and_utf16_offsets() {
    eval(
        &mut context(),
        r#"
      const text=B.from('abcabc');expect(text.indexOf('bc')===1&&text.lastIndexOf('bc')===4&&text.includes('bc',2),'string search');expect(text.indexOf(B.from('bc'),2)===4&&text.lastIndexOf(new Uint8Array([98,99]),2)===1,'byte view search');expect(text.indexOf('6263',0,'hex')===1&&text.indexOf('YmM=',0,'base64')===1,'encoded needle');
      const bytes=B.from([255,1,255]);expect(bytes.indexOf(-1)===0&&bytes.lastIndexOf(511)===2&&bytes.indexOf(257)===1,'number coercion');expect(bytes.indexOf(255,0,'bad')===0,'number ignores encoding');expect(text.indexOf(B.from('bc'),0,'bad')===1,'bytes ignore unknown encoding');
      const wide=B.from('abcabc','utf16le');expect(wide.indexOf('b',3,'utf16le')===2&&wide.lastIndexOf('b',3,'utf16le')===2,'UTF16 aligned unit search');expect(wide.indexOf('b',-1,'ucs2')===-1&&wide.lastIndexOf('b',-1,'ucs2')===8,'UTF16 remaining byte bound');expect(B.from('aa','utf16le').indexOf('a',3,'utf16le')===-1,'odd forward end offset');
      const odd=B.from([97,0,97,0,8]),pattern=B.from([97,0,8]);expect(odd.indexOf(pattern,2,'utf16le')===2&&odd.indexOf(pattern,3,'utf16le')===-1&&odd.lastIndexOf(pattern,4,'utf16le')===2,'odd byte count');expect(odd.indexOf(B.from([97]),0,'utf16le')===-1,'incomplete UTF16 needle');
      expect(text.indexOf('',-1)===5&&text.lastIndexOf('')===6&&text.indexOf('',Infinity)===6,'empty needle offsets');expect(text.indexOf('a',NaN)===0&&text.lastIndexOf('a',NaN)===3,'NaN offsets');expect(text.indexOf('a',[])===0&&text.lastIndexOf('a',{})===3,'offset coercion');
      const repeated=B.from('a'.repeat(5000)+'b');expect(repeated.indexOf('a'.repeat(2500)+'b')===2500&&repeated.lastIndexOf('a'.repeat(2500)+'b')===2500,'linear repeated-pattern search');
      throws(()=>text.indexOf('b',0,'bad'),'TypeError','ERR_UNKNOWN_ENCODING');throws(()=>text.indexOf([98]),'TypeError','ERR_INVALID_ARG_TYPE');
    "#,
    );
}

#[test]
fn ranged_compare_equals_swaps_and_borrowed_uint8array_methods() {
    eval(
        &mut context(),
        r#"
      const a=B.from('abcdef'),b=B.from('xxcdezz');expect(a.compare(b,2,5,2,5)===0,'ranged compare');expect(a.compare(b,1,0,0,2)===1&&a.compare(b,99,1,0,2)===1,'empty target range');expect(a.compare(b,0,1,99,6)===-1,'empty source range');expect(B.from([1]).equals(new Uint8Array([1])),'equals accepts Uint8Array');
      throws(()=>a.compare(b,'1',2),'TypeError','ERR_INVALID_ARG_TYPE');throws(()=>a.compare(b,-1,2),'RangeError','ERR_OUT_OF_RANGE');throws(()=>a.compare(b,0,99),'RangeError','ERR_OUT_OF_RANGE');
      const input=[0,1,2,3,4,5,6,7];for(const [bits,golden] of [[16,'0100030205040706'],[32,'0302010007060504'],[64,'0706050403020100']]){const bytes=B.from(input);expect(bytes['swap'+bits]()===bytes&&bytes.toString('hex')===golden,'swap '+bits);throws(()=>B.alloc(1)['swap'+bits](),'RangeError','ERR_INVALID_BUFFER_SIZE');}
      const borrowed=new Uint8Array([1,2,3,4]);expect(B.prototype.swap32.call(borrowed)===borrowed&&B.from(borrowed).toString('hex')==='04030201','borrowed swap');expect(B.prototype.readUInt32LE.call(new Uint8Array([0x78,0x56,0x34,0x12]))===0x12345678,'borrowed integer read');
      const float=new Uint8Array(4);B.prototype.writeFloatLE.call(float,1.5);expect(B.prototype.readFloatLE.call(float)===1.5,'borrowed float operations');expect(B.prototype.indexOf.call(new Uint8Array([1,2,3]),2)===1,'borrowed search');
    "#,
    );
}

#[test]
fn encoding_slice_write_methods_copy_bytes_and_inspection() {
    eval(
        &mut context(),
        r#"
      expect(B.from([233]).asciiSlice()==='i'&&B.from([233]).latin1Slice()==='é','ASCII/Latin1 slice');expect(B.from([251,255]).base64urlSlice()==='-_8','base64url slice');expect(B.from('A水','utf16le').ucs2Slice()==='A水','UTF16 slice');expect(B.from('abc').utf8Slice(Infinity)===''&&B.from('abc').hexSlice(99,99)==='','native slice empty boundary');throws(()=>B.from('abc').utf8Slice(1,99),'RangeError','ERR_OUT_OF_RANGE');
      const raw=B.alloc(3);expect(raw.hexWrite('616263',1,99)===2&&raw.toString('hex')==='006162','native write clips capacity');expect(B.alloc(3).base64Write('YWJj',NaN)===3,'base64 NaN offset');expect(B.alloc(3).asciiWrite('abc',.5)===2,'wrapped native fractional offset');throws(()=>B.alloc(3).utf8Write('abc',0,99),'RangeError','ERR_BUFFER_OUT_OF_BOUNDS');
      const unicode=B.alloc(3);expect(unicode.ucs2Write('ab')===2&&unicode.toString('hex')==='610000','native UTF16 boundary');expect(B.from('A水').toLocaleString('hex')==='41e6b0b4','locale alias');
      const wide=new Uint16Array([0x1234,0x5678]);expect(B.from(wide).toString('hex')==='3478'&&B.copyBytesFrom(wide).toString('hex')==='34127856','copyBytesFrom copies representation');expect(B.copyBytesFrom(wide,1,1).toString('hex')==='7856'&&B.copyBytesFrom(wide,3).length===0,'copyBytesFrom element range');throws(()=>B.copyBytesFrom(new DataView(wide.buffer)),'TypeError','ERR_INVALID_ARG_TYPE');throws(()=>B.copyBytesFrom(wide,0,.5),'RangeError','ERR_OUT_OF_RANGE');
      expect(B.alloc(0).inspect()==='<Buffer >'&&B.from([0,16,255]).inspect()==='<Buffer 00 10 ff>','inspect byte preview');expect(B.alloc(51,255).inspect().endsWith(' ... 1 more byte>'),'inspect preview cutoff');const extra=B.from([1]);extra.foo=7;extra.data={a:['str',true]};expect(extra.inspect(1,{})==="<Buffer 01, foo: 7, data: { a: [ 'str', true ] }>",'inspect own properties');
      const rich=B.from([1]);rich.extra=new Map([['a',1]]);expect(rich.inspect(1,{})==="<Buffer 01, extra: Map(1) { 'a' => 1 }>",'inspect Map');rich.extra={};rich.extra.self=rich.extra;expect(rich.inspect(1,{})==='<Buffer 01, extra: <ref *1> { self: [Circular *1] }>','inspect object cycle');rich.extra={[Symbol.for('nodejs.util.inspect.custom')]:()=> 'custom'};expect(rich.inspect(1,{})==='<Buffer 01, extra: custom>','custom inspect callback');rich.extra='1234567';expect(rich.inspect(1,{maxStringLength:3})==="<Buffer 01, extra: '123'... 4 more characters>",'inspect string limit');
      expect(B.alloc(4).fill([1,2]).toString('hex')==='00000000'&&B.alloc(4).fill(new Uint16Array([0x1234])).toString('hex')==='34123412','fill coercion and views');expect(B.alloc(2).fill('x',0,2,'').toString()==='xx','fill encoding default');
    "#,
    );
}

#[test]
fn inherited_typedarray_methods_preserve_node_return_types_and_boundaries() {
    eval(
        &mut context(),
        r#"
      const a=B.from([3,1,2]);expect(B.isBuffer(a.map(x=>x+1))&&a.map(x=>x+1).toString('hex')==='040203','map species');expect(B.isBuffer(a.filter(x=>x>1))&&a.filter(x=>x>1).toString('hex')==='0302','filter species');
      expect(!B.isBuffer(a.toReversed())&&a.toReversed() instanceof Uint8Array&&Array.from(a.toReversed()).join(',')==='2,1,3','toReversed Uint8Array');expect(!B.isBuffer(a.toSorted())&&Array.from(a.toSorted()).join(',')==='1,2,3','toSorted Uint8Array');expect(!B.isBuffer(a.with(-1,257))&&Array.from(a.with(-1,257)).join(',')==='3,1,1','with Uint8Array');throws(()=>a.with(99,1),'RangeError');
      expect(a.at(-1)===2&&a.findLast(x=>x>1)===2&&a.findLastIndex(x=>x>1)===2,'indexed readers');expect(a.every(x=>x>0)&&a.some(x=>x===1)&&a.reduce((x,y)=>x+y,0)===6&&a.reduceRight((x,y)=>x-y,10)===4,'predicates and reducers');expect(Array.from(a.keys()).join(',')==='0,1,2'&&Array.from(a.values()).join(',')==='3,1,2','iterators');
      const moved=B.from([1,2,3,4]);expect(moved.copyWithin(1,0,3)===moved&&moved.toString('hex')==='01010203','copyWithin overlap');expect(moved.reverse()===moved&&moved.sort()===moved,'in-place return values');throws(()=>moved.set([1],-1),'RangeError');
      const shared=a.subarray(1),slice=a.slice(1);expect(B.isBuffer(shared)&&B.isBuffer(slice),'view return types');shared[0]=8;expect(a[1]===8&&slice[0]===8,'views share backing');expect(a.subarray(-2).length===2&&a.subarray(Infinity).length===0,'view offset bounds');
    "#,
    );
}

#[test]
fn expanded_buffer_surface_runs_inside_actual_source_worker() {
    let script = r#"/* @name Buffer surface worker */
      lx.on(lx.EVENT_NAMES.request,async()=>{
        const B=lx.utils.buffer.from([]).constructor;
        const wide=B.from('abcabc','utf16le');if(wide.indexOf('b',3,'utf16le')!==2||wide.lastIndexOf('b',undefined,'utf16le')!==8)throw Error('UTF16 search');
        const bytes=B.from([0,1,2,3,4,5,6,7]);if(bytes.swap64().toString('hex')!=='0706050403020100')throw Error('swap64');
        if(B.copyBytesFrom(new Uint16Array([0x1234])).toString('hex')!=='3412')throw Error('typed representation');
        if(!B.isBuffer(bytes.map(x=>x))||B.isBuffer(bytes.toReversed()))throw Error('typed species');
        if(B.from([0,16,255]).inspect()!=='<Buffer 00 10 ff>')throw Error('inspect');
        if(B.from('abcdef').compare(B.from('xxcde'),2,5,2,5)!==0)throw Error('compare ranges');
        return 'http://127.0.0.1:9/buffer-surface.wav';
      });lx.send(lx.EVENT_NAMES.inited,{sources:{kw:{type:'music',actions:['musicUrl'],qualitys:['128k']}}});"#;
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
            .expect("Buffer worker response")
    };
    let init = exchange(json!({"op":"init","script":script}));
    let result = exchange(
        json!({"op":"request","request":{"source":"kw","action":"musicUrl","info":{"type":"128k","musicInfo":{"source":"kw","name":"Fixture","songmid":"fixture","singer":"Test","interval":"00:01","albumName":"","img":"","albumId":"","types":[],"_types":{},"typeUrl":{}}}}}),
    );
    let _ = child.kill();
    let _ = child.wait();
    assert_eq!(init["kind"], "initialized", "{init}");
    assert_eq!(
        result["url"], "http://127.0.0.1:9/buffer-surface.wav",
        "{result}"
    );
}
