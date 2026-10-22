// LX utility compatibility layer. No Node modules or ambient I/O are exposed.
(() => {
  'use strict';
  const native = globalThis.__lx_native_utils;
  delete globalThis.__lx_native_utils;
  const MAX = 2 * 1024 * 1024;
  const NativeUint8Array = Uint8Array;
  const NativeArrayBuffer = ArrayBuffer;
  const NativeDataView = DataView;
  const typedSubarray = NativeUint8Array.prototype.subarray;
  const NativePromise = Promise;
  const promiseResolve = NativePromise.resolve.bind(NativePromise);
  const Error = globalThis.Error;
  const TypeError = globalThis.TypeError;
  const RangeError = globalThis.RangeError;
  const stringify = JSON.stringify.bind(JSON);
  const fromCharCode = String.fromCharCode.bind(String);
  const isView = NativeArrayBuffer.isView.bind(NativeArrayBuffer);
  const call = (descriptor, data, key, iv) => native(stringify(descriptor), data, key, iv);
  function size(value, label = 'Buffer size') {
    if (typeof value !== 'number' || !Number.isInteger(value) || value < 0 || value > MAX)
      throw new RangeError(label + ' must be an integer in 0..2097152');
    return value;
  }
  function codedError(kind, code, message) { const error = new kind(message); error.code = code; return error; }
  function received(value) {
    if (typeof value === 'string') return "type string ('" + value + "')";
    if (typeof value === 'boolean') return 'type boolean (' + value + ')';
    if (typeof value === 'number') return 'type number (' + String(value) + ')';
    if (typeof value === 'bigint') return 'type bigint (' + displayNumber(value) + ')';
    if (typeof value === 'symbol') return 'type symbol (' + String(value) + ')';
    if (value === null) return 'null';
    if (typeof value === 'object') return 'an instance of ' + (Array.isArray(value) ? 'Array' : 'Object');
    return String(value);
  }
  function displayNumber(value) {
    const text = String(value);
    return typeof value === 'bigint' ? text.replace(/\B(?=(\d{3})+(?!\d))/g, '_') + 'n' : text;
  }
  function rangeError(name, requirement, value) { return codedError(RangeError, 'ERR_OUT_OF_RANGE', 'The value of "' + name + '" is out of range. It must be ' + requirement + '. Received ' + displayNumber(value)); }
  function numberType(value, name) { if (typeof value !== 'number') throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE', 'The "' + name + '" argument must be of type number. Received ' + received(value)); }
  function numericValue(value) { if (typeof value === 'bigint') throw new TypeError('Cannot convert a BigInt value to a number'); if (typeof value === 'symbol') throw new TypeError('Cannot convert a Symbol value to a number'); return +value; }
  function encoding(value, strict = false) {
    if (!strict && (typeof value !== 'string' || value === '')) return 'utf8';
    if (typeof value !== 'string' || value === '') throw codedError(TypeError, 'ERR_UNKNOWN_ENCODING', 'Unknown encoding: ' + String(value));
    switch (value.toLowerCase()) {
      case 'utf8': case 'utf-8': return 'utf8';
      case 'hex': return 'hex';
      case 'base64': return 'base64';
      case 'base64url': return 'base64url';
      case 'latin1': case 'binary': return 'latin1';
      case 'ascii': return 'ascii';
      case 'utf16le': case 'utf-16le': case 'ucs2': case 'ucs-2': return 'utf16le';
      default: throw codedError(TypeError, 'ERR_UNKNOWN_ENCODING', 'Unknown encoding: ' + value);
    }
  }
  function wellFormed(value) {
    // Rust's UTF-8 strings cannot carry lone JS UTF-16 surrogates. Buffer's
    // UTF-8 encoder replaces only those unpaired units with U+FFFD.
    if (!/[\ud800-\udfff]/.test(value)) return value;
    let result = '';
    for (let i = 0; i < value.length; i++) {
      const unit = value.charCodeAt(i);
      if (unit >= 0xd800 && unit <= 0xdbff) {
        const next = value.charCodeAt(i + 1);
        if (next >= 0xdc00 && next <= 0xdfff) result += value[i] + value[++i];
        else result += '\ufffd';
      } else if (unit >= 0xdc00 && unit <= 0xdfff) result += '\ufffd';
      else result += value[i];
    }
    return result;
  }
  function stringBytes(value, format) {
    const enc = encoding(format);
    if (enc === 'latin1' || enc === 'ascii' || enc === 'utf16le') {
      const result = new LxBuffer(size(value.length * (enc === 'utf16le' ? 2 : 1)));
      for (let i = 0; i < value.length; i++) {
        const unit = value.charCodeAt(i);
        if (enc === 'utf16le') { result[i * 2] = unit & 255; result[i * 2 + 1] = unit >>> 8; }
        else result[i] = unit & 255;
      }
      return result;
    }
    if (value.length > MAX * 4) throw new RangeError('Buffer string exceeds its bounded size limit');
    return new LxBuffer(call({ op: 'from', text: enc === 'utf8' ? wellFormed(value) : value, encoding: enc }));
  }
  function integer(value, fallback) {
    if (value === undefined) return fallback;
    const number = Number(value);
    if (Number.isNaN(number)) return 0;
    return Math.trunc(number);
  }
  function clampIndex(value, length, fallback) { return Math.max(0, Math.min(length, integer(value, fallback))); }
  function checkedIndex(value, length, label) {
    const name = label.split(' ').pop();
    numberType(value, name);
    if (!Number.isInteger(value)) throw rangeError(name, 'an integer', value);
    if (length < 0) throw codedError(RangeError, 'ERR_BUFFER_OUT_OF_BOUNDS', 'Attempt to access memory outside buffer bounds');
    if (value < 0 || value > length) throw rangeError(name, '>= 0 and <= ' + length, value);
    return value;
  }
  function byteLength(value) { numberType(value, 'byteLength'); if (!Number.isInteger(value) || value < 1 || value > 6) throw rangeError('byteLength', '>= 1 and <= 6', value); return value; }
  function copyIndex(value, fallback) { if (value === undefined) return fallback; const number = numericValue(value); return Number.isFinite(number) ? Math.floor(number) : 0; }
  function fillEncoding(value){if(value===undefined||value===null||value==='')return 'utf8';if(typeof value!=='string')throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "encoding" argument must be of type string. Received '+received(value));return encoding(value,true);}
  function allocationSize(value) { numberType(value, 'size'); if (Number.isNaN(value) || value < 0 || value > Number.MAX_SAFE_INTEGER) throw rangeError('size', '>= 0 && <= ' + Number.MAX_SAFE_INTEGER, value); if(value>MAX)throw rangeError('size','>= 0 && <= '+MAX,value);return Math.trunc(value); }
  function checkedReceiver(value) { if (!(value instanceof NativeUint8Array)) throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE', 'The "buffer" argument must be an instance of Buffer or Uint8Array. Received ' + received(value)); return value; }
  function checkedBytes(value,name){if(!(value instanceof NativeUint8Array))throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "'+name+'" argument must be an instance of Buffer or Uint8Array. Received '+received(value));return value;}
  function integerRange(value, name, min, max) { numberType(value, name); if (!Number.isInteger(value)) throw rangeError(name, 'an integer', value); if (value < min || value > max) throw rangeError(name, '>= ' + min + ' && <= ' + max, value); return value; }
  function search(buffer, value, offset, format, forward) {
    checkedReceiver(buffer);
    if (typeof offset === 'string') { format = offset; offset = undefined; }
    else if (offset > 0x7fffffff) offset = 0x7fffffff;
    else if (offset < -0x80000000) offset = -0x80000000;
    offset = numericValue(offset);
    if (Number.isNaN(offset)) offset = forward ? 0 : buffer.length;
    offset = Math.trunc(offset);
    let pattern, enc;
    if (typeof value === 'number') { pattern = new NativeUint8Array([value >>> 0 & 255]); enc = 'utf8'; }
    else if (typeof value === 'string') { enc = encoding(format, format !== undefined); pattern = stringBytes(value, enc); }
    else if (value instanceof NativeUint8Array) { pattern = value; try { enc = encoding(format, format !== undefined); } catch (_) { enc = 'utf8'; } }
    else throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE', 'The "value" argument must be of type number or string or an instance of Buffer or Uint8Array. Received ' + received(value));
    if (offset < 0) offset += buffer.length;
    if (!pattern.length) return Math.max(0, Math.min(buffer.length, offset));
    if (forward) { if (offset >= buffer.length) return -1; offset = Math.max(0, offset); if(offset>buffer.length-pattern.length)return -1; }
    else { if (offset < 0) return -1; offset = Math.min(buffer.length - 1, offset); }
    const width = enc === 'utf16le' ? 2 : 1, count = Math.floor(buffer.length / width), needle = Math.floor(pattern.length / width);
    if (!needle || !count || needle > count) return -1;
    offset = Math.floor(offset / width);
    const unit = (bytes, i) => width === 1 ? bytes[i] : bytes[i * 2] * 256 + bytes[i * 2 + 1];
    // Prefix matching keeps adversarial repeated patterns linear in both buffers.
    const prefix = new Int32Array(needle);
    for (let i = 1, matched = 0; i < needle; i++) { while (matched && unit(pattern, i) !== unit(pattern, matched)) matched = prefix[matched - 1]; if (unit(pattern, i) === unit(pattern, matched)) matched++; prefix[i] = matched; }
    const start = forward ? offset : 0, end = forward ? count : Math.min(count, offset + needle);
    let matched = 0, result = -1;
    for (let i = start; i < end; i++) { while (matched && unit(buffer, i) !== unit(pattern, matched)) matched = prefix[matched - 1]; if (unit(buffer, i) === unit(pattern, matched)) matched++; if (matched === needle) { const at = i - needle + 1; if (forward) return at * width; if (at <= offset) result = at * width; matched = prefix[matched - 1]; } }
    return result;
  }
  function directSlice(buffer, format, start, end) {
    checkedReceiver(buffer);start = start === undefined ? 0 : Math.trunc(numericValue(start));end = end === undefined ? buffer.length : Math.trunc(numericValue(end));if(Number.isNaN(start))start=0;if(Number.isNaN(end))end=0;
    if(start<0||end<0)throw codedError(RangeError,'ERR_OUT_OF_RANGE','Index out of range');if(end<=start)return '';
    if (end > buffer.length) throw codedError(RangeError, 'ERR_OUT_OF_RANGE', 'Index out of range');
    return LxBuffer.prototype.toString.call(buffer, format, start, end);
  }
  function directWrite(buffer, format, value, offset, length) {
    checkedReceiver(buffer);
    if (typeof value !== 'string') throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','argument must be a string');
    const defaultLength=length===undefined,wrapped=format==='ascii'||format==='latin1'||format==='utf8';
    offset = offset === undefined ? 0 : numericValue(offset);
    length = length === undefined ? buffer.length - offset : numericValue(length);
    if(wrapped){
      if(offset<0||offset>buffer.length)throw codedError(RangeError,'ERR_BUFFER_OUT_OF_BOUNDS','"offset" is outside of buffer bounds');
      if(length<0||length>buffer.length-offset)throw codedError(RangeError,'ERR_BUFFER_OUT_OF_BOUNDS','"length" is outside of buffer bounds');
    }
    offset=Number.isNaN(offset)?0:Math.trunc(offset);length=Number.isNaN(length)?0:Math.trunc(length);
    if(!wrapped&&defaultLength)length=buffer.length-offset;
    if(offset>buffer.length)throw codedError(RangeError,'ERR_BUFFER_OUT_OF_BOUNDS','"offset" is outside of buffer bounds');
    if(offset<0||length<0||!Number.isFinite(length))throw codedError(RangeError,'ERR_OUT_OF_RANGE','Index out of range');
    return LxBuffer.prototype.write.call(buffer,value,offset,Math.min(length,buffer.length-offset),format);
  }
  function inspectionString(value) {
    let quote="'";if(value.includes("'")&&!value.includes('"'))quote='"';else if(value.includes("'")&&value.includes('"')&&!value.includes('`'))quote='`';
    return quote+value.replace(/\\/g,'\\\\').replace(/\n/g,'\\n').replace(/\r/g,'\\r').replace(/\t/g,'\\t').replace(new RegExp(quote,'g'),'\\'+quote)+quote;
  }
  function inspectionValue(value,depth,options,seen) {
    const paint=(text,style)=>options.colors?'\x1b['+style+'m'+text+'\x1b['+(style===1?22:39)+'m':text;
    if(typeof value==='string'){const max=options.maxStringLength===null?value.length:Math.max(0,options.maxStringLength===undefined?10000:options.maxStringLength),tail=value.length>max?'... '+(value.length-max)+' more character'+(value.length-max===1?'':'s'):'';return paint(inspectionString(value.slice(0,max)),'32')+tail;}
    if(typeof value==='number')return paint(Object.is(value,-0)?'-0':String(value),'33');
    if(typeof value==='bigint')return paint(String(value)+'n','33');
    if(typeof value==='boolean')return paint(String(value),'33');
    if(value===undefined)return paint('undefined','90');if(value===null)return paint('null',1);if(typeof value==='symbol')return paint(String(value),'32');
    if(seen.has(value)){if(!seen.ids)seen.ids=new Map();if(!seen.ids.has(value))seen.ids.set(value,seen.ids.size+1);return paint('[Circular *'+seen.ids.get(value)+']','36');}
    const custom=value&&value[Symbol.for('nodejs.util.inspect.custom')];if(options.customInspect!==false&&typeof custom==='function'&&!(value instanceof LxBuffer)){const result=custom.call(value,depth,options,(child,childOptions)=>inspectionValue(child,childOptions&&childOptions.depth!==undefined?childOptions.depth:2,childOptions||{},new Set()));if(result!==value)return typeof result==='string'?result:inspectionValue(result,depth,options,seen);}
    if(typeof value==='function')return paint('[Function'+(value.name?': '+value.name:'')+']','36');
    if(value instanceof LxBuffer)return inspectBuffer(value,options,seen);
    if(value instanceof Date)return paint(Number.isNaN(value.getTime())?'Invalid Date':value.toISOString(),'35');if(value instanceof RegExp)return paint(String(value),'31');
    if(depth<0)return paint(Array.isArray(value)?'[Array]':'[Object]','36');seen.add(value);
    let result;
    if(Array.isArray(value)){const max=options.maxArrayLength===null?value.length:Math.max(0,options.maxArrayLength===undefined?100:options.maxArrayLength),rows=[];for(let i=0;i<Math.min(value.length,max);i++)rows.push(inspectionValue(value[i],depth-1,options,seen));if(value.length>max)rows.push('... '+(value.length-max)+' more item'+(value.length-max===1?'':'s'));result=rows.length?'[ '+rows.join(', ')+' ]':'[]';}
    else if(value instanceof Map){const rows=[];for(const [key,item]of value)rows.push(inspectionValue(key,depth-1,options,seen)+' => '+inspectionValue(item,depth-1,options,seen));result='Map('+value.size+') '+(rows.length?'{ '+rows.join(', ')+' }':'{}');}
    else if(value instanceof Set){const rows=[];for(const item of value)rows.push(inspectionValue(item,depth-1,options,seen));result='Set('+value.size+') '+(rows.length?'{ '+rows.join(', ')+' }':'{}');}
    else {const rows=[];for(const key of Reflect.ownKeys(value)){const descriptor=Object.getOwnPropertyDescriptor(value,key);if(!options.showHidden&&!descriptor.enumerable)continue;let label=typeof key==='symbol'?'['+String(key)+']':/^[A-Za-z_$][\w$]*$/.test(key)?key:inspectionString(key);if(!descriptor.enumerable&&typeof key!=='symbol')label='['+label+']';rows.push(label+': '+inspectionValue(value[key],depth-1,options,seen));}result=rows.length?'{ '+rows.join(', ')+' }':'{}';}
    seen.delete(value);return seen.ids&&seen.ids.has(value)?'<ref *'+seen.ids.get(value)+'> '+result:result;
  }
  function inspectBuffer(buffer,options,seen=new Set()){
    checkedReceiver(buffer);seen.add(buffer);const count=Math.min(buffer.length,50);let content=directSlice(buffer,'hex',0,count).replace(/(.{2})/g,'$1 ').trim();if(buffer.length>50)content+=' ... '+(buffer.length-50)+' more byte'+(buffer.length===51?'':'s');
    if(options){const extra=[],depth=options.depth===null?Infinity:options.depth===undefined?2:options.depth;for(const key of Reflect.ownKeys(buffer)){if(typeof key==='string'&&/^(?:0|[1-9]\d*)$/.test(key))continue;const descriptor=Object.getOwnPropertyDescriptor(buffer,key);if(!options.showHidden&&!descriptor.enumerable)continue;let label=typeof key==='symbol'?'['+String(key)+']':/^[A-Za-z_$][\w$]*$/.test(key)?key:inspectionString(key);if(!descriptor.enumerable&&typeof key!=='symbol')label='['+label+']';extra.push(label+': '+inspectionValue(buffer[key],depth-1,options,seen));}if(extra.length)content+=(buffer.length?', ':'')+extra.join(', ');}
    let name='Buffer';try{if(typeof buffer.constructor==='function'&&Object.prototype.hasOwnProperty.call(buffer.constructor,'name'))name=buffer.constructor.name;}catch(_){}seen.delete(buffer);return '<'+name+' '+content+'>';
  }
  function rawBytes(value, allowString = true, allowArrayBuffer = true) {
    if (allowString && typeof value === 'string') return stringBytes(value, 'utf8');
    if (isView(value)) return new NativeUint8Array(value.buffer, value.byteOffset, value.byteLength);
    if (allowArrayBuffer && value instanceof NativeArrayBuffer) return new NativeUint8Array(value);
    throw new TypeError('Expected a string, Buffer, TypedArray or DataView');
  }
  function characters(units) {
    let result = '';
    // Keep calls well below the interpreter's stack and argument limits.
    for (let start = 0; start < units.length; start += 512)
      result += fromCharCode(...units.slice(start, start + 512));
    return result;
  }
  class LxBuffer extends NativeUint8Array {
    constructor(value = 0, byteOffset, length) {
      if (typeof value === 'number') { super(size(value)); return; }
      if (value instanceof NativeArrayBuffer) {
        let rawOffset=byteOffset===undefined?0:numericValue(byteOffset);if(Number.isNaN(rawOffset))rawOffset=0;
        const maximum=value.byteLength-rawOffset;
        if(maximum<0)throw codedError(RangeError,'ERR_BUFFER_OUT_OF_BOUNDS','"offset" is outside of buffer bounds');
        let rawCount=length===undefined?undefined:numericValue(length);if(rawCount!==undefined){if(rawCount>0&&rawCount>maximum)throw codedError(RangeError,'ERR_BUFFER_OUT_OF_BOUNDS','"length" is outside of buffer bounds');if(!(rawCount>0))rawCount=0;}
        const offset=Math.trunc(rawOffset),count=rawCount===undefined?value.byteLength-offset:Math.trunc(rawCount);
        if(offset<0)throw new RangeError('Start offset '+offset+' is outside the bounds of the buffer');
        size(count);
        super(value, offset, count); return;
      }
      if (value === null || value === undefined || typeof value.length !== 'number')
        throw new TypeError('Buffer input must be a byte array or ArrayBuffer');
      const count = Number.isNaN(value.length) ? 0 : size(Math.max(0, Math.trunc(value.length)));
      if (isView(value) && !(value instanceof NativeUint8Array)) {
        // Buffer.from takes one low byte per numeric element. Explicit modulo
        // conversion also avoids the engine's saturating wider-array cast.
        super(count);
        for (let index = 0; index < count; index++) this[index] = value[index] & 255;
        return;
      }
      super(value);
    }
    static from(value, encodingOrOffset, length) {
      if (typeof value === 'string') return stringBytes(value, encodingOrOffset);
      if (value instanceof NativeArrayBuffer) return new LxBuffer(value, encodingOrOffset, length);
      if (value && typeof value === 'object') {
        const converted = typeof value.valueOf === 'function' ? value.valueOf() : value;
        if (converted != null && converted !== value) return LxBuffer.from(converted, encodingOrOffset, length);
        if (value.length!==undefined||value.buffer instanceof NativeArrayBuffer) return typeof value.length === 'number' ? new LxBuffer(value) : new LxBuffer(0);
        if (value.type === 'Buffer' && Array.isArray(value.data)) return new LxBuffer(value.data);
        if (typeof value[Symbol.toPrimitive] === 'function')
          return LxBuffer.from(value[Symbol.toPrimitive]('string'), encodingOrOffset, length);
      }
      throw new TypeError('Buffer.from requires a string, array, Buffer or ArrayBuffer');
    }
    static isBuffer(value) { return value instanceof LxBuffer; }
    static isEncoding(value) { try { if (typeof value !== 'string' || !value) return false; encoding(value); return true; } catch (_) { return false; } }
    static alloc(length, fill = 0, format) { const count = allocationSize(length); return new LxBuffer(count).fill(fill, 0, count, format); }
    // The host deliberately zeroes allocUnsafe rather than exposing prior memory.
    static allocUnsafe(length) { return new LxBuffer(allocationSize(length)); }
    static allocUnsafeSlow(length) { return new LxBuffer(allocationSize(length)); }
    static of(...values) { const result = new LxBuffer(size(values.length)); for (let i = 0; i < values.length; i++) result[i] = values[i]; return result; }
    static copyBytesFrom(view, offset, length) {
      if (!isView(view) || view instanceof NativeDataView) throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "view" argument must be an instance of TypedArray. Received ' + received(view));
      if (!view.length) return new LxBuffer(0);
      const start = offset === undefined ? 0 : integerRange(offset,'offset',0,Number.MAX_SAFE_INTEGER);
      if (start >= view.length) return new LxBuffer(0);
      const count = length === undefined ? view.length - start : Math.min(integerRange(length,'length',0,Number.MAX_SAFE_INTEGER),view.length-start),width=view.byteLength/view.length;
      return new LxBuffer(new NativeUint8Array(view.buffer,view.byteOffset+start*width,size(count*width)));
    }
    static byteLength(value, format) {
      if (value instanceof NativeArrayBuffer || isView(value)) return value.byteLength;
      if (typeof value !== 'string') throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "string" argument must be of type string or an instance of Buffer or ArrayBuffer. Received '+received(value));
      let enc;
      try { enc = encoding(format); } catch (_) { enc = 'utf8'; }
      if (enc === 'hex') return Math.floor(value.length / 2);
      if (enc === 'latin1' || enc === 'ascii') return value.length;
      if (enc === 'utf16le') return value.length * 2;
      if (enc === 'base64' || enc === 'base64url') {
        let length = value.length;
        if (length && value[length - 1] === '=') length--;
        if (length && value[length - 1] === '=') length--;
        return Math.floor(length * 3 / 4);
      }
      return stringBytes(value, enc).length;
    }
    static concat(list, totalLength) {
      if(!Array.isArray(list))throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "list" argument must be an instance of Array. Received '+received(list));
      if(!list.length)return new LxBuffer(0);
      if(totalLength!==undefined)integerRange(totalLength,'length',0,Number.MAX_SAFE_INTEGER);
      list.forEach((value,i)=>checkedBytes(value,'list['+i+']'));
      const total = totalLength === undefined ? list.reduce((sum, value) => sum + value.length, 0) : totalLength;
      const result = new LxBuffer(allocationSize(total));
      let offset = 0;
      for (const value of list) {
        if (offset >= total) break;
        const count = Math.min(value.length, total - offset);
        result.set(value.subarray(0, count), offset); offset += count;
      }
      return result;
    }
    static compare(left, right) {
      checkedBytes(left,'buf1');checkedBytes(right,'buf2');
      const length = Math.min(left.length, right.length);
      for (let index = 0; index < length; index++) if (left[index] !== right[index]) return left[index] < right[index] ? -1 : 1;
      return left.length === right.length ? 0 : left.length < right.length ? -1 : 1;
    }
    toString(format, start, end) {
      const range = typedSubarray.call(this,clampIndex(start, this.length, 0), clampIndex(end, this.length, this.length));
      if (!range.length) return '';
      const enc = encoding(format, format !== undefined);
      if (enc === 'utf16le') {
        const units = [];
        for (let i = 0; i + 1 < range.length; i += 2) units.push(range[i] | (range[i + 1] << 8));
        return characters(units);
      }
      return call({ op: 'toString', encoding: enc }, range);
    }
    toJSON() { return { type: 'Buffer', data: Array.from(this) }; }
    subarray(start, end) {
      checkedReceiver(this);const normalize = value => { let n = Math.trunc(numericValue(value)); if (Number.isNaN(n)) n = 0; if (n < 0) n += this.length; return Math.max(0,Math.min(this.length,n)); };
      const first=normalize(start),last=end===undefined?this.length:normalize(end);return new LxBuffer(this.buffer,this.byteOffset+first,Math.max(0,last-first));
    }
    slice(start, end) { return LxBuffer.prototype.subarray.call(this,start,end); }
    equals(other) { checkedBytes(other,'otherBuffer');return LxBuffer.compare(this, other) === 0; }
    compare(other,targetStart=0,targetEnd=other&&other.length,sourceStart=0,sourceEnd=this.length) {
      checkedReceiver(this);if (!(other instanceof NativeUint8Array)) throw codedError(TypeError,'ERR_INVALID_ARG_TYPE','The "target" argument must be an instance of Buffer or Uint8Array. Received ' + received(other));
      integerRange(targetStart,'targetStart',0,Number.MAX_SAFE_INTEGER);integerRange(targetEnd,'targetEnd',0,other.length);integerRange(sourceStart,'sourceStart',0,Number.MAX_SAFE_INTEGER);integerRange(sourceEnd,'sourceEnd',0,this.length);
      if(sourceStart>=sourceEnd)return targetStart>=targetEnd?0:-1;if(targetStart>=targetEnd)return 1;
      return LxBuffer.compare(typedSubarray.call(this,sourceStart,sourceEnd),typedSubarray.call(other,targetStart,targetEnd));
    }
    indexOf(value,offset,format) { return search(this,value,offset,format,true); }
    lastIndexOf(value,offset,format) { return search(this,value,offset,format,false); }
    includes(value,offset,format) { return search(this,value,offset,format,true)!==-1; }
    inspect(_depth,options) { return inspectBuffer(this,options); }
    get parent() { return LxBuffer.isBuffer(this)?this.buffer:undefined; }
    get offset() { return LxBuffer.isBuffer(this)?this.byteOffset:undefined; }
    write(value, offset = 0, length = this.length - offset, format) {
      if (typeof value !== 'string') throw new TypeError('Buffer.write value must be a string');
      if (typeof offset === 'string') { format = offset; offset = 0; length = this.length; }
      else if (typeof length === 'string') { format = length; length = this.length - offset; }
      checkedIndex(offset, this.length, 'Buffer.write offset');
      checkedIndex(length, this.length, 'length');
      const enc = encoding(format, !!format), encoded = stringBytes(value, enc);
      let count = Math.min(encoded.length, length, this.length - offset);
      // Node's writes avoid partial UTF-8 sequences / UTF-16 code units.
      if (enc === 'utf8' && count < encoded.length) {
        while (count > 0 && (encoded[count] & 0xc0) === 0x80) count--;
      }
      if (enc === 'utf16le') count -= count % 2;
      this.set(encoded.subarray(0, count), offset);
      return count;
    }
    copy(target, targetStart = 0, sourceStart = 0, sourceEnd = this.length) {
      if (!(target instanceof NativeUint8Array)) throw new TypeError('Buffer.copy target must be Buffer/Uint8Array');
      targetStart = copyIndex(targetStart, 0); sourceStart = copyIndex(sourceStart, 0); sourceEnd = copyIndex(sourceEnd, this.length);
      if (targetStart < 0) throw rangeError('targetStart', '>= 0', targetStart);
      if (sourceStart < 0 || sourceStart > this.length) throw rangeError('sourceStart', '>= 0 && <= ' + this.length, sourceStart);
      if (sourceEnd < 0) throw rangeError('sourceEnd', '>= 0', sourceEnd);
      const count = Math.max(0, Math.min(target.length - targetStart, Math.min(this.length, sourceEnd) - sourceStart));
      if (count) target.set(this.subarray(sourceStart, sourceStart + count), targetStart);
      return count;
    }
    fill(value, offset = 0, end = this.length, format) {
      if(typeof value==='string'){
        if (typeof offset === 'string') { format = offset; offset = 0; end = this.length; }
        else if (typeof end === 'string') { format = end; end = this.length; }
        format=fillEncoding(format);
      }
      integerRange(offset,'offset',0,Number.MAX_SAFE_INTEGER);integerRange(end,'end',0,this.length);
      if (end <= offset) return this;
      if (typeof value === 'string' && value.length === 0) value = 0;
      if (typeof value !== 'string' && !isView(value))
        return NativeUint8Array.prototype.fill.call(this,numericValue(value) & 255, offset, end);
      const pattern = typeof value === 'string' ? stringBytes(value, format) : rawBytes(value, false, false);
      if (!pattern.length) throw new TypeError('Buffer.fill value is empty');
      for (let i = offset; i < end; i++) this[i] = pattern[(i - offset) % pattern.length];
      return this;
    }
    readUIntLE(offset, byteLength) { return numericOps._read.call(this,offset, byteLength, false, false); }
    readUIntBE(offset, byteLength) { return numericOps._read.call(this,offset, byteLength, true, false); }
    readIntLE(offset, byteLength) { return numericOps._read.call(this,offset, byteLength, false, true); }
    readIntBE(offset, byteLength) { return numericOps._read.call(this,offset, byteLength, true, true); }
    _read(offset, byteLength, bigEndian, signed) {
      byteLength = validateByteLength(byteLength);
      checkedIndex(offset, this.length - byteLength, 'Buffer read offset');
      let result = 0;
      for (let i = 0; i < byteLength; i++) result += this[offset + (bigEndian ? byteLength - i - 1 : i)] * (2 ** (8 * i));
      if (signed && result >= 2 ** (8 * byteLength - 1)) result -= 2 ** (8 * byteLength);
      return result;
    }
    writeUIntLE(value, offset, byteLength) { return numericOps._write.call(this,value, offset, byteLength, false, false); }
    writeUIntBE(value, offset, byteLength) { return numericOps._write.call(this,value, offset, byteLength, true, false); }
    writeIntLE(value, offset, byteLength) { return numericOps._write.call(this,value, offset, byteLength, false, true); }
    writeIntBE(value, offset, byteLength) { return numericOps._write.call(this,value, offset, byteLength, true, true); }
    _write(value, offset, byteLength, bigEndian, signed) {
      byteLength = validateByteLength(byteLength);
      value = numericValue(value);
      const limit = 2 ** (8 * byteLength), min = signed ? -limit / 2 : 0, max = signed ? limit / 2 - 1 : limit - 1;
      if (value < min || value > max) throw rangeError('value', '>= ' + min + ' and <= ' + max, value);
      checkedIndex(offset, this.length - byteLength, 'Buffer write offset');
      value = Math.trunc(value);
      if (value < 0) value += limit;
      for (let i = 0; i < byteLength; i++) this[offset + (bigEndian ? byteLength - i - 1 : i)] = Math.floor(value / 2 ** (8 * i)) & 255;
      return offset + byteLength;
    }
    _readFloat(offset, bytes, littleEndian) {
      checkedIndex(offset, this.length - bytes, 'offset');
      const view = new NativeDataView(this.buffer, this.byteOffset, this.byteLength);
      return bytes === 4 ? view.getFloat32(offset, littleEndian) : view.getFloat64(offset, littleEndian);
    }
    _writeFloat(value, offset, bytes, littleEndian) {
      value = numericValue(value);
      checkedIndex(offset, this.length - bytes, 'offset');
      const view = new NativeDataView(this.buffer, this.byteOffset, this.byteLength);
      if (bytes === 4) view.setFloat32(offset, value, littleEndian); else view.setFloat64(offset, value, littleEndian);
      return offset + bytes;
    }
    _readBig(offset, littleEndian, signed) {
      checkedIndex(offset, this.length - 8, 'offset');
      let result = 0n;
      for (let i = 0; i < 8; i++) result = (result << 8n) | BigInt(this[offset + (littleEndian ? 7 - i : i)]);
      return signed && result >= (1n << 63n) ? result - (1n << 64n) : result;
    }
    _writeBig(value, offset, littleEndian, signed) {
      const min = signed ? -(1n << 63n) : 0n, max = signed ? (1n << 63n) - 1n : (1n << 64n) - 1n;
      if (value < min || value > max) throw rangeError('value', signed ? '>= -(2n ** 63n) and < 2n ** 63n' : '>= 0n and < 2n ** 64n', value);
      checkedIndex(offset, this.length - 8, 'offset');
      let bits;
      try { bits = value & ((1n << 64n) - 1n); } catch (_) { throw new TypeError('Cannot mix BigInt and other types, use explicit conversions'); }
      for (let i = 0; i < 8; i++) { this[offset + (littleEndian ? i : 7 - i)] = Number(bits & 255n); bits >>= 8n; }
      return offset + 8;
    }
  }
  const numericOps = {};
  for(const name of ["_read","_write","_readFloat","_writeFloat","_readBig","_writeBig"]){numericOps[name]=LxBuffer.prototype[name];delete LxBuffer.prototype[name];}
  const validateByteLength = byteLength;
  for (const [bits, bytes] of [[8, 1], [16, 2], [32, 4]]) {
    for (const signed of [false, true]) {
      const kind = signed ? 'Int' : 'UInt';
      for (const endian of bits === 8 ? [''] : ['LE', 'BE']) {
        Object.defineProperty(LxBuffer.prototype, 'read' + kind + bits + endian, {
          value(offset = 0) { return numericOps._read.call(this,offset, bytes, endian === 'BE', signed); }, configurable: true, writable: true,
        });
        Object.defineProperty(LxBuffer.prototype, 'write' + kind + bits + endian, {
          value(value, offset = 0) { return numericOps._write.call(this,value, offset, bytes, endian === 'BE', signed); }, configurable: true, writable: true,
        });
      }
    }
  }
  for (const [kind, bytes] of [['Float', 4], ['Double', 8]]) {
    for (const endian of ['LE', 'BE']) {
      Object.defineProperty(LxBuffer.prototype, 'read' + kind + endian, { value(offset = 0) { return numericOps._readFloat.call(this,offset, bytes, endian === 'LE'); }, configurable: true, writable: true });
      Object.defineProperty(LxBuffer.prototype, 'write' + kind + endian, { value(value, offset = 0) { return numericOps._writeFloat.call(this,value, offset, bytes, endian === 'LE'); }, configurable: true, writable: true });
    }
  }
  for (const signed of [false, true]) {
    const kind = signed ? 'BigInt64' : 'BigUInt64';
    for (const endian of ['LE', 'BE']) {
      Object.defineProperty(LxBuffer.prototype, 'read' + kind + endian, { value(offset = 0) { return numericOps._readBig.call(this,offset, endian === 'LE', signed); }, configurable: true, writable: true });
      Object.defineProperty(LxBuffer.prototype, 'write' + kind + endian, { value(value, offset = 0) { return numericOps._writeBig.call(this,value, offset, endian === 'LE', signed); }, configurable: true, writable: true });
    }
  }
  for (const name of Object.getOwnPropertyNames(LxBuffer.prototype)) {
    if (/^(read|write)(UInt|BigUInt)/.test(name)) Object.defineProperty(LxBuffer.prototype, name.replace('UInt', 'Uint'), { value: LxBuffer.prototype[name], configurable: true, writable: true });
  }
  for(const [name,format] of [['ascii','ascii'],['base64','base64'],['base64url','base64url'],['latin1','latin1'],['hex','hex'],['ucs2','utf16le'],['utf8','utf8']]){
    Object.defineProperty(LxBuffer.prototype,name+'Slice',{value(start,end){return directSlice(this,format,start,end);},configurable:true,writable:true});
    Object.defineProperty(LxBuffer.prototype,name+'Write',{value(value,offset,length){return directWrite(this,format,value,offset,length);},configurable:true,writable:true});
  }
  for(const bytes of [2,4,8])Object.defineProperty(LxBuffer.prototype,'swap'+bytes*8,{value(){checkedReceiver(this);if(this.length%bytes)throw codedError(RangeError,'ERR_INVALID_BUFFER_SIZE','Buffer size must be a multiple of '+bytes*8+'-bits');for(let start=0;start<this.length;start+=bytes)for(let i=0;i<bytes/2;i++){const value=this[start+i];this[start+i]=this[start+bytes-1-i];this[start+bytes-1-i]=value;}return this;},configurable:true,writable:true});
  Object.defineProperty(LxBuffer.prototype,'toLocaleString',{value:LxBuffer.prototype.toString,configurable:true,writable:true});
  Object.defineProperty(LxBuffer.prototype,Symbol.for('nodejs.util.inspect.custom'),{value:LxBuffer.prototype.inspect,configurable:true,writable:true});
  function Buffer(value,encodingOrOffset,length){return typeof value==='number'?new LxBuffer(allocationSize(value)):LxBuffer.from(value,encodingOrOffset,length);}
  Object.setPrototypeOf(Buffer,NativeUint8Array);Buffer.prototype=LxBuffer.prototype;Object.defineProperty(LxBuffer.prototype,'constructor',{value:Buffer,configurable:true,writable:true});
  for(const name of Object.getOwnPropertyNames(LxBuffer))if(!['name','length','prototype'].includes(name))Object.defineProperty(Buffer,name,Object.getOwnPropertyDescriptor(LxBuffer,name));
  Object.defineProperty(LxBuffer,'name',{value:'FastBuffer',configurable:true});Object.defineProperty(Buffer,Symbol.species,{get(){return LxBuffer;},configurable:true});Buffer.poolSize=65536;
  Object.defineProperty(Buffer,Symbol('kIsEncodingSymbol'),{value:Buffer.isEncoding,configurable:true,writable:true,enumerable:true});
  for(const name of Object.getOwnPropertyNames(LxBuffer.prototype)){
    const descriptor=Object.getOwnPropertyDescriptor(LxBuffer.prototype,name);if(typeof descriptor.value!=='function'||name==='constructor')continue;
    let methodName=name==='toLocaleString'?'toString':name.replace('Uint','UInt');if(/^(ascii|base64|base64url|latin1|hex|ucs2|utf8)(Slice|Write)$/.test(name)||/^read(?:UInt|Uint|Int)(?:8|16|32)/.test(name))methodName='';
    if(/^(read|write)(Float|Double)(LE|BE)$/.test(name))methodName=name.slice(0,-2)+(name.endsWith('LE')?'Forwards':'Backwards');
    Object.defineProperty(descriptor.value,'name',{value:methodName,configurable:true});
    const arities={copy:4,compare:5,fill:4,write:4,toString:3,toLocaleString:3},arity=/^read(?:UInt|Uint|Int)(?:8|16|32)/.test(name)?1:Object.prototype.hasOwnProperty.call(arities,name)?arities[name]:undefined;if(arity!==undefined)Object.defineProperty(descriptor.value,'length',{value:arity,configurable:true});
  }
  Object.defineProperty(Buffer.alloc,'length',{value:3,configurable:true});
  for(const name of Reflect.ownKeys(LxBuffer.prototype)){if(name==='constructor')continue;const descriptor=Object.getOwnPropertyDescriptor(LxBuffer.prototype,name);descriptor.enumerable=true;if(name==='parent'||name==='offset'){Object.defineProperty(descriptor.get,'name',{value:'get',configurable:true});descriptor.configurable=false;}Object.defineProperty(LxBuffer.prototype,name,descriptor);}
  for(const name of Object.getOwnPropertyNames(Buffer))if(!['name','length','prototype'].includes(name)){const descriptor=Object.getOwnPropertyDescriptor(Buffer,name);descriptor.enumerable=true;Object.defineProperty(Buffer,name,descriptor);}
  const buffer = Object.freeze({
    from: (...args) => LxBuffer.from(...args),
    bufToString: (value, format) => LxBuffer.from(value, 'binary').toString(format),
  });
  const crypto = Object.freeze({
    md5(value) { return call({ op: 'md5' }, rawBytes(value, true, false)); },
    randomBytes(length) { return new LxBuffer(call({ op: 'random', size: size(length, 'randomBytes size') })); },
    aesEncrypt(value, mode, key, iv) {
      if (typeof mode !== 'string') throw new TypeError('AES mode must be a string');
      return new LxBuffer(call({ op: 'aes', mode }, rawBytes(value, true, false), rawBytes(key), iv === null ? new NativeUint8Array(0) : rawBytes(iv)));
    },
    rsaEncrypt(value, key) {
      if (!(value instanceof NativeUint8Array)) throw codedError(TypeError, 'ERR_INVALID_ARG_TYPE', 'RSA input must be a Buffer or Uint8Array');
      if (value.length > 128) throw rangeError('size', '>= 0 and <= 2097152', 128 - value.length);
      const keyBytes = typeof key === 'string' ? stringBytes(key, 'utf8') : rawBytes(key, false);
      return new LxBuffer(call({ op: 'rsa' }, value, keyBytes));
    },
  });
  const zlib = Object.freeze({
    inflate(value) { return promiseResolve().then(() => new LxBuffer(call({ op: 'inflate' }, rawBytes(value, true, false)))); },
    deflate(value) { return promiseResolve().then(() => new LxBuffer(call({ op: 'deflate' }, rawBytes(value, true, false)))); },
  });
  Object.defineProperty(globalThis, '__lxHostUtils', { configurable: true, writable: false,
    value: Object.freeze({ utils: Object.freeze({ buffer, crypto, zlib }), Buffer }) });
})();
