// CGUI-13 (TERM #536): keyboard-activation helper for custom-clickable elements.
// A native <button>/<a> fires its click on Enter/Space for free; a clickable <div>/<span>
// does not. Any such element that carries an onClick must also be reachable (tabIndex) and
// operable — this returns the onKeyDown handler that makes Enter and Space behave like a
// click, so the interaction pattern stays identical to the ModuleCard reference.
import type { KeyboardEvent } from 'react';

export function onEnterOrSpace<E extends HTMLElement | SVGElement>(fn: () => void) {
  return (e: KeyboardEvent<E>) => {
    if (e.key === 'Enter' || e.key === ' ' || e.key === 'Spacebar') {
      e.preventDefault();
      fn();
    }
  };
}
