/**
 * @name Local Tone Catalog
 * @description Resolves project-owned loopback tones
 * @version 1.0.0
 * @author LX GPUI Player contributors
 * @homepage https://example.invalid/lx-gpui-player
 */
// SPDX-License-Identifier: CC0-1.0
// Original fixture code dedicated to the public domain under CC0 1.0.
// Uses only the player's local catalog of synthesized, project-owned audio.

(() => {
  "use strict";

  const required = [
    "name", "singer", "source", "songmid", "interval", "albumName", "img",
    "typeUrl", "albumId", "types", "_types", "filePath", "ext"
  ];

  function validate(request) {
    if (request.source !== "local" || request.action !== "musicUrl") {
      throw new Error("Local Tone Catalog supports only local musicUrl");
    }
    if (!request.info || request.info.type !== null) {
      throw new Error("Local musicUrl requires info.type to be null");
    }
    const music = request.info.musicInfo;
    if (!music || typeof music !== "object") {
      throw new Error("Missing legacy musicInfo object");
    }
    required.forEach((key) => {
      if (!Object.prototype.hasOwnProperty.call(music, key)) {
        throw new Error("Missing legacy musicInfo." + key);
      }
    });
    if (music.source !== "local" || music.albumId !== "" ||
        !Array.isArray(music.types) || music.types.length !== 0 ||
        !music._types || typeof music._types !== "object" ||
        !music.typeUrl || typeof music.typeUrl !== "object" ||
        typeof music.songmid !== "string" || typeof music.ext !== "string") {
      throw new Error("Invalid local legacy musicInfo fields");
    }
    if (typeof music.filePath !== "string" ||
        !/^http:\/\/127\.0\.0\.1:\d+\//.test(music.filePath)) {
      throw new Error("Fixture accepts only the player's loopback audio URLs");
    }
    return music;
  }

  function getCatalog(url) {
    return new Promise((resolve, reject) => {
      const cancel = lx.request(url, { method: "get", timeout: 5000 },
        (error, response, body) => {
          if (error) {
            reject(new Error("Local catalog request failed: " +
              (error.message || String(error))));
            return;
          }
          if (!response || response.statusCode !== 200) {
            reject(new Error("Local catalog returned HTTP " +
              (response ? response.statusCode : "unknown")));
            return;
          }
          if (!Array.isArray(body)) {
            reject(new Error("Local catalog JSON must be an array"));
            return;
          }
          resolve(body);
        });
      if (typeof cancel !== "function") {
        reject(new Error("lx.request must return a cancellation function"));
      }
    });
  }

  lx.on(lx.EVENT_NAMES.request, (request) => Promise.resolve().then(() => {
    const music = validate(request);
    const base = music.filePath.substring(0, music.filePath.lastIndexOf("/") + 1);
    return getCatalog(base + "catalog.json").then((catalog) => {
      const track = catalog.find((entry) => entry.id === music.songmid);
      if (!track || typeof track.url !== "string" ||
          track.url.indexOf(base) !== 0) {
        throw new Error("Local catalog has no matching loopback track: " + music.songmid);
      }
      return track.url;
    });
  }));

  lx.send(lx.EVENT_NAMES.inited, {
    sources: {
      local: {
        name: "Local Tone Catalog", type: "music",
        actions: ["musicUrl"], qualitys: []
      }
    }
  });
})();
