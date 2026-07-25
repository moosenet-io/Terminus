// CGUI-13 (TERM #536): keyboard-activation helper for custom-clickable elements.
// A native <button>/<a> fires its click on Enter/Space for free; a clickable <div>/<span>
// does not. Any such element that carries an onClick must also be reachable (tabIndex) and
// operable — this returns the onKeyDown handler that makes Enter and Space behave like a
// click, so the interaction pattern stays identical to the ModuleCard reference.
//
// CGUI-13 review fix (codex + free, High): the handler must ONLY act when the key event
// originated on the element the handler is attached to — NOT when it bubbled up from a nested
// interactive control (a <button>, <a>, <input>, toggle, or any inner focusable). Keyboard
// events target the focused element, so `e.target === e.currentTarget` is true exactly when the
// container itself is focused, and false when a descendant is. Without this guard, pressing
// Space inside a nested <input> would both block typing (preventDefault) AND fire the card
// action, and Enter/Space on a nested <button> would double-fire (button + card). The guard is
// a no-op for leaf callers (e.g. the Forest control spans), where target already === currentTarget.
import type { KeyboardEvent } from 'react';

export function onEnterOrSpace<E extends HTMLElement | SVGElement>(fn: () => void) {
  return (e: KeyboardEvent<E>) => {
    if (e.target !== e.currentTarget) return; // bubbled from a nested control — let it own the key
    if (e.key === 'Enter' || e.key === ' ' || e.key === 'Spacebar') {
      e.preventDefault();
      fn();
    }
  };
}
