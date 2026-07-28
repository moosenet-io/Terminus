// CGUI-12 (TERM #535): the fixed deep-space backdrop (guide §0) — mounted once by the shell,
// behind everything (z-index:0), so the gradient + nebula radials + starfield stay put while
// the canvas scrolls in front of them ("the frame never moves; only the canvas scrolls").
//
// All the actual visual treatment (the green-tinted vertical gradient, the two nebula radials,
// the six-dot starfield, and the reduced-motion-respecting twinkle) lives in the `.const-backdrop`
// rule in globals.css — this component is just the mount point. It renders nothing interactive
// and is aria-hidden.
export function DeepSpaceBackdrop() {
  return <div className="const-backdrop" aria-hidden />;
}
