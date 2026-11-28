const preferenceKey = "tempo-v2-board-sounds";
const cueNames = ["move-self", "move-opponent", "capture", "check", "castle", "promote", "illegal"];
const maxCueDelayMs = 100;
const maxVoices = 2;

export function moveCue(move, board, ordinary = "move-self") {
  if (board.isCheck()) return "check";
  if (move.isPromotion()) return "promote";
  if (move.isKingsideCastle() || move.isQueensideCastle()) return "castle";
  if (move.isCapture() || move.isEnPassant()) return "capture";
  return ordinary;
}

export function createBoardSounds() {
  let enabled = true;
  try { enabled = localStorage.getItem(preferenceKey) !== "false"; }
  catch { /* Keep sound usable when browser storage is unavailable. */ }

  let context;
  let resuming;
  let generation = 0;
  const playing = new Set();
  const clips = new Map();

  function preload() {
    if (!enabled) return;
    for (const cue of cueNames) {
      if (clips.has(cue)) continue;
      const recording = fetch(`/assets/${cue}.mp3`).then((response) => {
        if (!response.ok) throw new Error("Audio is unavailable");
        return response.arrayBuffer();
      });
      clips.set(cue, { recording });
      void recording.catch(() => clips.delete(cue));
    }
  }

  function unlock() {
    if (!enabled) return;
    try {
      context ??= new AudioContext();
      if (context.state !== "running") resuming = context.resume().catch(() => {});
      preload();
      for (const [cue, clip] of clips) {
        if (clip.decoding) continue;
        clip.decoding = clip.recording.then((bytes) => context.decodeAudioData(bytes)).then((buffer) => {
          clip.buffer = buffer;
          return buffer;
        });
        void clip.decoding.catch(() => {
          if (clips.get(cue) === clip) clips.delete(cue);
        });
      }
    } catch { /* Sound is optional. */ }
  }

  function stop(player) {
    playing.delete(player);
    try { player.stop(); } catch { /* Sound must never block a transition. */ }
  }

  function cancel() {
    generation++;
    for (const player of playing) stop(player);
  }

  function start(buffer) {
    try {
      // An immediate reply can share the player's attack. Never queue a clip.
      if (playing.size >= maxVoices) stop(playing.values().next().value);
      const player = context.createBufferSource();
      player.buffer = buffer;
      const volume = context.createGain();
      volume.gain.value = 0.72;
      player.connect(volume).connect(context.destination);
      player.onended = () => {
        player.disconnect();
        volume.disconnect();
        playing.delete(player);
      };
      player.start();
      playing.add(player);
    } catch { /* Sound is optional. */ }
  }

  function play(cue) {
    if (!enabled || !context) return;
    const playback = ++generation;
    const clip = clips.get(cue);
    if (clip?.buffer && context.state === "running") {
      start(clip.buffer);
      return;
    }
    if (!clip?.decoding) return;
    const began = performance.now();
    void Promise.all([clip.decoding, context.state === "running" ? undefined : resuming]).then(([decoded]) => {
      if (playback === generation && context.state === "running" &&
          performance.now() - began <= maxCueDelayMs) start(decoded);
    }).catch(() => {});
  }

  return {
    get enabled() { return enabled; },
    setEnabled(value) {
      enabled = value;
      try { localStorage.setItem(preferenceKey, String(value)); }
      catch { /* Apply the preference in this tab. */ }
      if (enabled) unlock();
      else cancel();
    },
    preload,
    unlock,
    play,
    cancel,
  };
}
