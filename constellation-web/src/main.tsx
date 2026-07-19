import React from 'react'
import ReactDOM from 'react-dom/client'
// CONST-17 r4: the token sheet MUST be applied before any module in App's graph evaluates —
// theme.ts memoizes a getComputedStyle pass, so importing App first could permanently cache
// the non-browser fallback hexes for the whole page session (codex review finding).
import './styles/fonts.css'
import './styles/globals.css'
import './styles/interactions.css'
import App from './App'
// Side-effect import: registers every panel with the module registry before the shell renders.
import './panels/registerPanels'

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>
)
