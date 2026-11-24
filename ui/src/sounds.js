const preferenceKey = "tempo-v2-board-sounds";

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
  let current;
  let generation = 0;
  const clips = new Map();

  function unlock() {
    if (!enabled) return;
    try {
      context ??= new AudioContext();
      if (context.state !== "running") void context.resume().catch(() => {});
    } catch { /* Sound is optional. */ }
  }

  function cancel() {
    generation++;
    try { current?.stop(); } catch { /* Sound must never block a transition. */ }
    current = undefined;
  }

  function play(cue) {
    if (!enabled || !context) return;
    cancel();
    const playback = generation;
    if (!clips.has(cue)) {
      const clip = fetch(`/assets/${cue}.mp3`)
        .then((response) => {
          if (!response.ok) throw new Error("Audio is unavailable");
          return response.arrayBuffer();
        })
        .then((bytes) => context.decodeAudioData(bytes));
      clips.set(cue, clip);
      void clip.catch(() => clips.delete(cue));
    }
    void clips.get(cue).then((buffer) => {
      if (!enabled || playback !== generation || context.state !== "running") return;
      const player = context.createBufferSource();
      player.buffer = buffer;
      const volume = context.createGain();
      volume.gain.value = 0.72;
      player.connect(volume).connect(context.destination);
      player.onended = () => {
        player.disconnect();
        volume.disconnect();
        if (current === player) current = undefined;
      };
      player.start();
      current = player;
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
    unlock,
    play,
    cancel,
  };
}
