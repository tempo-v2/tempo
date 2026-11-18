import { spawn } from 'node:child_process';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

// Application transport adapter. The UI package never imports child_process.
export function stdioTransport(binary = process.env.TEMPO_BINARY ?? fileURLToPath(new URL('../../target/debug/tempo', import.meta.url))) {
  const child = spawn(binary, ['stdio'], { stdio: ['pipe', 'pipe', 'pipe'] });
  const pending = [];
  let failure;
  let stderr = '';
  child.stderr.setEncoding('utf8').on('data', chunk => { stderr += chunk; });
  const lines = createInterface({ input: child.stdout });
  lines.on('line', line => {
    const request = pending.shift();
    if (!request) return;
    clearTimeout(request.timer);
    try { request.resolve(JSON.parse(line)); } catch (error) { request.reject(error); }
  });
  const exited = new Promise(resolve => {
    child.on('error', error => { failure = error; for (const request of pending.splice(0)) { clearTimeout(request.timer); request.reject(error); } resolve(); });
    child.on('exit', code => {
      failure = new Error(`Tempo exited (${code}): ${stderr}`);
      for (const request of pending.splice(0)) { clearTimeout(request.timer); request.reject(failure); }
      resolve();
    });
  });
  return {
    send(command) {
      if (failure) return Promise.reject(failure);
      return new Promise((resolve, reject) => {
        const request = { resolve, reject, timer: setTimeout(() => { child.kill(); reject(new Error('Tempo reply timeout')); }, 10000) };
        pending.push(request);
        child.stdin.write(`${JSON.stringify(command)}\n`);
      });
    },
    async close() { child.stdin.end(); await exited; lines.close(); },
  };
}
