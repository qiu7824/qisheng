/**
 * @name Local Tone Direct
 * @description Resolves supplied project-owned tones
 * @version 1.0.0
 * @author LX GPUI Player contributors
 * @homepage https://example.invalid/lx-gpui-player
 */
// SPDX-License-Identifier: CC0-1.0
// Original fixture code dedicated to the public domain under CC0 1.0.
// No network request is made by this fixture.

(() => {
  "use strict";
  const required = [
    "name", "singer", "source", "songmid", "interval", "albumName", "img",
    "typeUrl", "albumId", "types", "_types", "filePath", "ext"
  ];
  lx.on(lx.EVENT_NAMES.request, (request) => Promise.resolve().then(() => {
    if (request.source !== "local" || request.action !== "musicUrl") {
      throw new Error("Local Tone Direct supports only local musicUrl");
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
    return music.filePath;
  }));
  lx.send(lx.EVENT_NAMES.inited, {
    sources: {
      local: {
        name: "Local Tone Direct", type: "music",
        actions: ["musicUrl"], qualitys: []
      }
    }
  });
})();
