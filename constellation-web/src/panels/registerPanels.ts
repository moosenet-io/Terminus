// CONST-04: Central import point that registers every panel module with the module registry.
// Imported once, for side effects only, from src/main.tsx before the app renders. Each future
// panel (CONST-05..12) adds one line here — the shell never needs to change.
import { registerPanel } from '../lib/moduleRegistry';
import { TerminusPanel } from './terminus/TerminusPanel';

registerPanel({
  id: 'terminus.config',
  system: 'Terminus',
  title: 'Config',
  path: '/terminus/config',
  icon: '⚙',
  available: true,
  component: TerminusPanel,
});
