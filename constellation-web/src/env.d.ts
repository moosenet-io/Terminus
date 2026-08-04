// RMCP-13 (TERM-624): a MINIMAL ambient type for the two build-time flags Vite statically
// replaces, so a `import.meta.env.PROD` guard can be written literally — which is what makes it
// eliminable.
//
// Deliberately NOT `/// <reference types="vite/client" />`: that would also type
// `import.meta.glob`, which several test files legitimately reach through an `@ts-expect-error`
// (this project has no vite/client types by convention). Typing it here would turn those
// suppressions into "unused @ts-expect-error" errors — a widening change to unrelated files for
// no benefit. Two fields is all this needs.
//
// The literal form matters: Vite replaces the exact text `import.meta.env.PROD` at transform
// time, so a cast-wrapped or optional-chained read (`(import.meta as …).env?.PROD`) would NOT be
// replaced and would not fold to a constant — and the whole point of the guard is that it folds.
interface ImportMetaEnv {
  /** `true` in a production build (`vite build`), `false` in dev and under vitest. */
  readonly PROD: boolean;
  /** The inverse of `PROD`. */
  readonly DEV: boolean;
}

interface ImportMeta {
  readonly env: ImportMetaEnv;
}
