// CGUI-13 (TERM #536): shared live `prefers-reduced-motion` hook.
// The CSS media rule in globals.css stills every *CSS* animation app-wide, but SVG SMIL
// (`<animate>`/`<animateMotion>`) and JS-interval streams are NOT reachable by CSS — those
// must be withheld in render code. This hook is the one source of truth for that guard
// (Forest sap/halo, ModuleDetail flow pulse, TaskTree node halos all consult it). It tracks
// the media query live so toggling the OS setting updates the UI without a reload.
import { useEffect, useState } from 'react';

export function useReducedMotion(): boolean {
  const [reduced, setReduced] = useState<boolean>(() =>
    typeof matchMedia !== 'undefined' && matchMedia('(prefers-reduced-motion: reduce)').matches);
  useEffect(() => {
    if (typeof matchMedia === 'undefined') return;
    const mq = matchMedia('(prefers-reduced-motion: reduce)');
    const onChange = () => setReduced(mq.matches);
    mq.addEventListener('change', onChange);
    return () => mq.removeEventListener('change', onChange);
  }, []);
  return reduced;
}
