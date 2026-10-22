/**
 * @name Local POST + Utils
 * @description Original loopback tone resolver using LX HTTP and utilities
 * @version 1.0.0
 * @author LX Native Lab
 */
// SPDX-License-Identifier: CC0-1.0
// Original fixture code dedicated to the public domain under CC0 1.0.
// Uses only this project's synthesized tones and its loopback POST endpoint.

(() => {
  "use strict";

  const fields = [
    "name", "singer", "source", "songmid", "interval", "albumName", "img",
    "typeUrl", "albumId", "types", "_types", "filePath", "ext"
  ];

  function validate(request) {
    if (request.source !== "local" || request.action !== "musicUrl") {
      throw new Error("Local POST + Utils supports only local musicUrl");
    }
    if (!request.info || request.info.type !== null) {
      throw new Error("Local musicUrl requires info.type to be null");
    }
    const music = request.info.musicInfo;
    if (!music || typeof music !== "object") {
      throw new Error("Missing legacy musicInfo object");
    }
    fields.forEach((field) => {
      if (!Object.prototype.hasOwnProperty.call(music, field)) {
        throw new Error("Missing legacy musicInfo." + field);
      }
    });
    if (music.source !== "local" || music.albumId !== "" || music.ext !== "wav" ||
        !Array.isArray(music.types) || music.types.length !== 0 ||
        !music._types || typeof music._types !== "object" ||
        Object.keys(music._types).length !== 0 ||
        !music.typeUrl || typeof music.typeUrl !== "object" ||
        Object.keys(music.typeUrl).length !== 0 ||
        typeof music.songmid !== "string" || !/^tone-[123]$/.test(music.songmid)) {
      throw new Error("Expected complete local legacy fields for an original demo tone");
    }
    if (typeof music.filePath !== "string" ||
        !/^http:\/\/127\.0\.0\.1:\d+\/tone-[123]\.wav$/.test(music.filePath)) {
      throw new Error("Fixture accepts only the player's original loopback tone URLs");
    }
    const base = music.filePath.substring(0, music.filePath.lastIndexOf("/") + 1);
    if (music.filePath !== base + music.songmid + ".wav") {
      throw new Error("Legacy songmid does not match the loopback tone URL");
    }
    return { music, base };
  }

  function postResolver(url, payload) {
    return new Promise((resolve, reject) => {
      const cancel = lx.request(url, {
        method: "post", timeout: 5000,
        headers: {
          "Content-Type": "application/json; charset=utf-8",
          "X-Fixture": "lx-http-utils-v1"
        },
        body: JSON.stringify(payload)
      }, (error, response, body) => {
        if (error) {
          reject(new Error("Local POST failed: " + (error.message || String(error))));
          return;
        }
        if (!response || response.statusCode !== 200) {
          reject(new Error("Local resolver returned HTTP " +
            (response ? response.statusCode : "unknown") +
            (body && body.error ? ": " + body.error : "")));
          return;
        }
        if (!body || typeof body !== "object" || typeof body.url !== "string") {
          reject(new Error("Local resolver must return a JSON object with a URL"));
          return;
        }
        resolve(body);
      });
      if (typeof cancel !== "function") {
        reject(new Error("lx.request must return a cancellation function"));
      }
    });
  }

  lx.on(lx.EVENT_NAMES.request, async (request) => {
    const { music, base } = validate(request);
    const buffer = lx.utils.buffer;
    const plain = buffer.from(music.songmid, "utf8");
    const compressed = await lx.utils.zlib.deflate(plain);
    const restored = await lx.utils.zlib.inflate(compressed);
    if (buffer.bufToString(restored, "utf8") !== music.songmid) {
      throw new Error("LX zlib round-trip changed the original tone ID");
    }
    // Original public test bytes, each exactly 16 UTF-8 bytes; not credentials.
    const key = buffer.from("LXFixtureKey2026", "utf8");
    const iv = buffer.from("LoopbackIV2026!!", "utf8");
    const encrypted = lx.utils.crypto.aesEncrypt(plain, "aes-128-cbc", key, iv);
    const payload = {
      id: music.songmid,
      checksum: lx.utils.crypto.md5(music.songmid),
      cipher: buffer.bufToString(encrypted, "base64"),
      compressed: buffer.bufToString(compressed, "base64")
    };
    const response = await postResolver(base + "resolve", payload);
    if (response.id !== music.songmid || response.url !== music.filePath ||
        response.url.indexOf(base) !== 0) {
      throw new Error("Local resolver returned a different or non-loopback tone");
    }
    return response.url;
  });

  lx.send(lx.EVENT_NAMES.inited, {
    sources: {
      local: {
        name: "Local POST + Utils", type: "music",
        actions: ["musicUrl"], qualitys: []
      }
    }
  });
})();
