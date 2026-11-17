import { createStudyController } from '../ui/index.js';
import { stdioTransport } from './stdio-transport.mjs';

const transport = stdioTransport();
let number = 0;
const ui = createStudyController({
  transport,
  render() {},
  storage: { load: () => null, save() {} },
  makeSubmissionId: () => `demo-input-${++number}`,
});
const show = view => console.log(`${view.target_id ?? 'queue'}: ${view.status}; assessment=${view.assessment ?? 'none'}${view.feedback ? `; ${view.feedback.message}` : ''}`);
try {
  console.log('Tempo interface demo — real application, in-memory store, fixture chess/scheduler policies');
  show(await ui.next()); show(await ui.answer('e2e4'));
  show(await ui.next()); show(await ui.answer('g1f3'));
  show(await ui.next()); show(await ui.answer('d2d4'));
  show(await ui.answer('e2e4')); show(await ui.answer('g1f3'));
  console.log('New attempt for the missed tactic:');
  show(await ui.next()); show(await ui.answer('e2e4')); show(await ui.answer('g1f3'));
  show(await ui.next());
} finally { await transport.close(); }
