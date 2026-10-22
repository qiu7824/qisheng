// SPDX-License-Identifier: CC0-1.0
// Verifies this project's authored fixture behavior with a minimal LX API mock.
// The Rust runtime tests separately verify actual Boa execution and validation.
"use strict";
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

function load(name, catalog) {
  const state = { handler: null, initialized: null, requests: [] };
  const lx = {
    version: "2.0.0", env: "desktop",
    EVENT_NAMES: { request: "request", inited: "inited", updateAlert: "updateAlert" },
    on(event, handler) {
      assert.equal(event, "request");
      state.handler = handler;
      return Promise.resolve();
    },
    send(event, data) {
      assert.equal(event, "inited");
      assert.equal(state.initialized, null, "inited must be sent once");
      state.initialized = data;
      return Promise.resolve();
    },
    request(url, options, callback) {
      state.requests.push({ url, options });
      let cancelled = false;
      queueMicrotask(() => {
        if (!cancelled) callback(null, { statusCode: 200, body: catalog }, catalog);
      });
      return () => { cancelled = true; };
    }
  };
  const source = fs.readFileSync(path.join(__dirname, name), "utf8");
  assert.match(source, /^\/\*\*[\s\S]*?\* @name /);
  new vm.Script(source, { filename: name }).runInNewContext({ lx });
  return state;
}

function request(overrides = {}) {
  return {
    source: "local", action: "musicUrl",
    info: {
      type: null,
      musicInfo: {
        name: "First Light", singer: "LX Native Lab", source: "local",
        songmid: "tone-1", interval: "00:45", albumName: "Native Tones",
        img: "", typeUrl: {}, albumId: "", types: [], _types: {},
        filePath: "http://127.0.0.1:12345/tone-1.wav", ext: "wav",
        ...overrides
      }
    }
  };
}

async function main() {
  const fixtureRequest = request();
  const url = fixtureRequest.info.musicInfo.filePath;
  const catalog = [{ id: "tone-1", title: "First Light", url }];
  for (const name of ["legal-local-source.js", "legal-direct-source.js"]) {
    const state = load(name, catalog);
    assert.equal(state.initialized.sources.local.type, "music");
    assert.equal(state.initialized.sources.local.actions.join(","), "musicUrl");
    assert.equal(state.initialized.sources.local.qualitys.length, 0);
    const result = state.handler(fixtureRequest);
    assert.equal(typeof result.then, "function", "handler returns a Promise");
    assert.equal(await result, url);
    await assert.rejects(state.handler(request({ filePath: "https://thirdparty.invalid/audio.wav" })), /loopback/);
    const wrongType = request();
    wrongType.info.type = "128k";
    await assert.rejects(state.handler(wrongType), /type to be null/);
    for (const key of Object.keys(fixtureRequest.info.musicInfo)) {
      const missing = request();
      delete missing.info.musicInfo[key];
      await assert.rejects(state.handler(missing), new RegExp("Missing legacy musicInfo\\." + key));
    }
    if (name === "legal-local-source.js") {
      assert.equal(state.requests[0].url, "http://127.0.0.1:12345/catalog.json");
      assert.equal(state.requests[0].options.method, "get");
      assert.equal(state.requests[0].options.timeout, 5000);
      await assert.rejects(load(name, []).handler(fixtureRequest), /no matching/);
      await assert.rejects(load(name, { tracks: catalog }).handler(fixtureRequest), /must be an array/);
      await assert.rejects(load(name, [{ ...catalog[0], url: "https://thirdparty.invalid/audio.wav" }]).handler(fixtureRequest), /no matching/);
    } else {
      assert.equal(state.requests.length, 0);
    }
  }
  assert.throws(() => load("broken-source.js"), (error) => error.name === "SyntaxError");
  assert.throws(() => load("init-throw-source.js"), /initialization failed intentionally/);
  const noHandler = load("no-handler-source.js");
  assert.ok(noHandler.initialized);
  assert.equal(noHandler.handler, null);
  const wrongUrl = load("wrong-url-source.js");
  assert.equal(await wrongUrl.handler(fixtureRequest), "file:///forbidden.wav");
  console.log("PASS: 6 fixtures, legacy fields, local catalog callback, Promise contract, and deliberate error cases.");
}

main().catch((error) => { console.error(error); process.exitCode = 1; });
