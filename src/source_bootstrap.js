// Original host adapter for the LX v2 custom-source contract. No Node or browser I/O.
(() => {
  'use strict';
  const nativeStart = globalThis.__lx_http_start;
  const nativeCancel = globalThis.__lx_http_cancel;
  const nativePoll = globalThis.__lx_http_poll;
  delete globalThis.__lx_http_start;
  delete globalThis.__lx_http_cancel;
  delete globalThis.__lx_http_poll;
  const nativeTimerSchedule = globalThis.__lx_timer_schedule;
  const nativeTimerCancel = globalThis.__lx_timer_cancel;
  const nativeTimerPoll = globalThis.__lx_timer_poll;
  const nativeHostEvent = globalThis.__lx_host_event;
  delete globalThis.__lx_timer_schedule;
  delete globalThis.__lx_timer_cancel;
  delete globalThis.__lx_timer_poll;
  delete globalThis.__lx_host_event;
  const hostUtils = globalThis.__lxHostUtils;
  delete globalThis.__lxHostUtils;
  const callbacks = new Map();
  const timers = new Map();
  const mapGet = Function.prototype.call.bind(Map.prototype.get);
  const mapSet = Function.prototype.call.bind(Map.prototype.set);
  const mapDelete = Function.prototype.call.bind(Map.prototype.delete);
  const mapClear = Function.prototype.call.bind(Map.prototype.clear);
  const mapEntries = Function.prototype.call.bind(Map.prototype.entries);
  const iteratorNext = Function.prototype.call.bind(Object.getPrototypeOf(callbacks.entries()).next);
  let epoch = 0;
  const currentScriptInfo = Object.freeze(globalThis.__lx_script_info);
  delete globalThis.__lx_script_info;
  const jsonParse = JSON.parse.bind(JSON);
  const jsonStringify = JSON.stringify.bind(JSON);
  const NativePromise = Promise;
  const NativeError = Error;
  const NativeNumber = Number;
  const nativeTrunc = Math.trunc.bind(Math);
  // Sources also use the ordinary console surface outside the LX API.
  // There is no developer console in the isolated worker. Discard diagnostic
  // arguments without formatting them, retaining them, or forwarding them to
  // stdout (which carries the worker protocol).
  const discardDiagnostic = () => undefined;
  globalThis.console = Object.freeze(Object.fromEntries([
    'log', 'info', 'warn', 'error', 'debug', 'trace', 'dir', 'dirxml', 'table',
    'group', 'groupCollapsed', 'groupEnd', 'clear', 'count', 'countReset',
    'time', 'timeLog', 'timeEnd', 'assert', 'profile', 'profileEnd', 'timeStamp',
  ].map(name => [name, discardDiagnostic])));
  const state = { initialized: false, manifest: null, handler: null, error: null,
    response: null, responseError: null, settled: false, requestAction: null, updateAlertSent: false };
  const names = Object.freeze({ request: 'request', inited: 'inited', updateAlert: 'updateAlert' });
  const supportedSources = ['kw', 'kg', 'tx', 'wy', 'mg', 'local'];
  const qualities = ['128k', '320k', 'flac', 'flac24bit'];
  const fail = message => new NativeError(message);
  const lx = {
    version: '2.0.0', env: 'desktop', currentScriptInfo, EVENT_NAMES: names,
    on(event, handler) {
      if (event !== names.request) return NativePromise.reject(fail('The event is not supported: ' + event));
      if (typeof handler !== 'function') return NativePromise.reject(fail('Request handler must be a function'));
      state.handler = handler;
      return NativePromise.resolve();
    },
    send(event, data) {
      if (event === names.updateAlert) {
        if (state.updateAlertSent) return NativePromise.reject(fail('The update alert can only be called once.'));
        state.updateAlertSent = true;
        try {
          if (!data || typeof data !== 'object') throw fail('parameter format error.');
          if (!data.log || typeof data.log !== 'string') throw fail('log is required.');
          let log = data.log;
          if (log.length > 1024) {
            log = log.slice(0, 1024);
            const last = log.charCodeAt(log.length - 1);
            if (last >= 0xd800 && last <= 0xdbff) log = log.slice(0, -1);
            log += '...';
          }
          const updateUrl = typeof data.updateUrl === 'string' && data.updateUrl.length <= 1024
            && /^https?:\/\/[^\s]+$/.test(data.updateUrl) ? data.updateUrl : null;
          nativeHostEvent(jsonStringify({ event: 'update_alert', log, update_url: updateUrl }));
          return NativePromise.resolve();
        } catch (error) { return NativePromise.reject(error); }
      }
      if (event !== names.inited) return NativePromise.reject(fail('The event is not supported: ' + event));
      if (state.initialized) {
        state.error = 'Script is inited';
        return NativePromise.reject(fail(state.error));
      }
      state.initialized = true;
      try {
        if (!data || !data.sources || typeof data.sources !== 'object') throw fail('Missing required parameter init info.sources');
        if (data.openDevTools === true) nativeHostEvent(jsonStringify({ event: 'open_dev_tools_requested' }));
        const sources = {};
        for (const key of supportedSources) {
          const item = data.sources[key];
          if (!item || item.type !== 'music') continue;
          if (!Array.isArray(item.actions) || !Array.isArray(item.qualitys)) throw fail('Invalid actions/qualitys for source ' + key);
          const allowed = key === 'local' ? ['musicUrl', 'lyric', 'pic'] : ['musicUrl'];
          sources[key] = { type: 'music', actions: allowed.filter(action => item.actions.includes(action)),
            qualitys: key === 'local' ? [] : qualities.filter(quality => item.qualitys.includes(quality)) };
        }
        if (Object.keys(sources).length === 0) throw fail('No supported music sources were declared');
        state.manifest = { sources };
        return NativePromise.resolve();
      } catch (error) {
        state.error = error && error.message ? error.message : String(error);
        return NativePromise.reject(error);
      }
    },
    request(url, options, callback) {
      if (typeof callback !== 'function') throw fail('lx.request callback must be a function');
      let envelope;
      let cancelled = false;
      const record = { callback, cancelled: false, epoch };
      try {
        const serialized = jsonStringify(options || {}, (key, value) =>
          ArrayBuffer.isView(value) ? { __lxBytes: Array.from(new Uint8Array(value.buffer, value.byteOffset, value.byteLength)) } : value);
        envelope = jsonParse(nativeStart(url, serialized));
      } catch (error) { envelope = { error: error && error.message ? error.message : String(error) }; }
      if (envelope.error) {
        NativePromise.resolve().then(() => { if (!cancelled && record.epoch === epoch) callback.call(lx, fail(envelope.error), null, null); });
      } else { mapSet(callbacks, envelope.id, record); }
      return () => {
        if (cancelled) return;
        cancelled = true;
        record.cancelled = true;
        if (envelope.id !== undefined) { mapDelete(callbacks, envelope.id); nativeCancel(envelope.id); }
      };
    },
    utils: hostUtils.utils,
  };
  Object.freeze(lx);
  Object.defineProperty(globalThis, 'lx', { value: lx, writable: false, configurable: false });
  const scheduleTimer = (callback, delay, repeating, args) => {
    if (typeof callback !== 'function') throw new TypeError('Timer callback must be a function');
    const id = nativeTimerSchedule(delay === undefined ? 0 : delay, repeating);
    mapSet(timers, id, { callback, repeating, args, cancelled: false, epoch });
    return id;
  };
  const clearTimer = id => {
    id = nativeTrunc(NativeNumber(id));
    const record = mapGet(timers, id);
    if (record) record.cancelled = true;
    mapDelete(timers, id);
    nativeTimerCancel(id);
  };
  for (const [name, value] of [
    ['setTimeout', (callback, delay, ...args) => scheduleTimer(callback, delay, false, args)],
    ['setInterval', (callback, delay, ...args) => scheduleTimer(callback, delay, true, args)],
    ['clearTimeout', clearTimer], ['clearInterval', clearTimer],
  ]) Object.defineProperty(globalThis, name, { value, writable: false, configurable: false });
  Object.defineProperty(globalThis, '__lxHost', { configurable: false, writable: false,
    value: Object.freeze({
      clearRequests() {
        const entries = mapEntries(callbacks);
        for (let entry = iteratorNext(entries); !entry.done; entry = iteratorNext(entries)) {
          entry.value[1].cancelled = true;
          nativeCancel(entry.value[0]);
        }
        mapClear(callbacks);
      },
      clearAsync() {
        epoch++;
        this.clearRequests();
        const entries = mapEntries(timers);
        for (let entry = iteratorNext(entries); !entry.done; entry = iteratorNext(entries)) {
          entry.value[1].cancelled = true;
          nativeTimerCancel(entry.value[0]);
        }
        mapClear(timers);
      },
      pollTimers() {
        for (const id of jsonParse(nativeTimerPoll())) {
          const record = mapGet(timers, id);
          if (!record || record.cancelled || record.epoch !== epoch) continue;
          if (!record.repeating) mapDelete(timers, id);
          record.callback.apply(globalThis, record.args);
        }
      },
      isComplete(kind) {
        return kind === 'init' ? state.error !== null || (state.initialized && state.manifest !== null && typeof state.handler === 'function') : state.settled;
      },
      pollRequests() {
        for (const envelope of jsonParse(nativePoll())) {
          const record = mapGet(callbacks, envelope.id);
          mapDelete(callbacks, envelope.id);
          if (!record) continue;
          NativePromise.resolve().then(() => {
            if (record.cancelled || record.epoch !== epoch) return;
            const callback = record.callback;
            if (envelope.error) callback.call(lx, fail(envelope.error), null, null);
            else {
              const response = envelope.response;
              response.raw = hostUtils.Buffer.from(response.raw, 'base64');
              callback.call(lx, null, response, response.body);
            }
          });
        }
      },
      manifest() {
        if (state.error) throw fail(state.error);
        if (!state.initialized || !state.manifest) throw fail('Source did not send the inited event');
        if (typeof state.handler !== 'function') throw fail('Request event is not defined');
        return jsonStringify(state.manifest);
      },
      dispatch(request) {
        this.manifest();
        state.response = null; state.responseError = null; state.settled = false;
        state.requestAction = request.action;
        const requestEpoch = epoch;
        try {
          const result = state.handler.call(lx, request);
          if (!result || typeof result.then !== 'function') throw fail('Request handler must return a Promise/thenable');
          NativePromise.resolve(result).then(value => {
            if (requestEpoch !== epoch) return;
            state.response = value; state.settled = true;
          }, error => {
            if (requestEpoch !== epoch) return;
            state.responseError = error && error.message ? error.message : String(error);
            state.settled = true;
          });
        } catch (error) {
          state.responseError = error && error.message ? error.message : String(error);
          state.settled = true;
        }
      },
      responseError() { return JSON.stringify(state.responseError); },
      result() {
        if (!state.settled) throw fail('Source request did not settle; unresolved asynchronous work');
        if (state.responseError !== null) throw fail(state.responseError);
        if (state.requestAction === 'lyric') {
          const value = state.response;
          if (!value || typeof value !== 'object' || Array.isArray(value)) throw fail('Local lyric result must be an object');
          const fields = ['lyric', 'tlyric', 'rlyric', 'lxlyric'];
          const limits = { lyric: 51200, tlyric: 5120, rlyric: 5120, lxlyric: 8192 };
          const result = Object.create(null);
          for (const key of Object.keys(value)) if (!fields.includes(key)) throw fail('Unknown local lyric field: ' + key);
          for (const key of fields) {
            const field = value[key];
            if (key === 'lyric' && typeof field !== 'string') throw fail('Local lyric result.lyric must be a string');
            if (field === undefined) continue;
            if (field !== null && typeof field !== 'string') throw fail('Local lyric result.' + key + ' must be string, null or missing');
            if (typeof field === 'string') {
              if (key === 'lyric' ? field.length > limits[key] : field.length >= limits[key])
                throw fail('Local lyric result.' + key + ' exceeds the LX UTF-16 length limit');
              if (field.includes('\u0000')) throw fail('Lyric text contains a NUL character');
            }
            result[key] = field;
          }
          return jsonStringify({ result });
        }
        if (typeof state.response !== 'string' || state.response.length > 2048)
          throw fail('musicUrl/pic handler must resolve to a string of at most 2048 UTF-16 units');
        return jsonStringify({ result: state.response });
      },
    }) });
})();
