// All inline SVG icon strings used across the UI.
// Each icon is sized so the rendered pixel dimensions differ from its
// container's inner size by an even number on each axis (whole-pixel
// placement at DPR 1 with flex centering).
// dsprack.js icons (ICON_POWER, ICON_SOLO, ICON_GEAR, ICON_GEAR_SM) are
// exported here but applied by WP-B.

export const ICON_FOLDER =
  `<svg width="12" height="10" viewBox="0 0 14 12" fill="none"
       stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round"
       aria-hidden="true">
    <path d="M1 3h4l1.5-2H13a1 1 0 0 1 1 1v8a1 1 0 0 1-1 1H1a1 1 0 0 1-1-1V4a1 1 0 0 1 1-1z"/>
  </svg>`;

export const ICON_X_SM =
  `<svg width="8" height="8" viewBox="0 0 10 10" fill="none"
       stroke="currentColor" stroke-width="1.6" stroke-linecap="round"
       aria-hidden="true">
    <line x1="1.5" y1="1.5" x2="8.5" y2="8.5"/>
    <line x1="8.5" y1="1.5" x2="1.5" y2="8.5"/>
  </svg>`;

export const ICON_CONVERT =
  `<svg width="10" height="9" viewBox="0 0 14 12" fill="none"
       stroke="currentColor" stroke-width="1.5" stroke-linecap="round"
       aria-hidden="true">
    <path d="M13 3.5A5.6 5.6 0 1 0 13 8.5"/>
  </svg>`;

export const ICON_PLAY_SM =
  `<svg width="7" height="8" viewBox="0 0 8 10" fill="currentColor"
       aria-hidden="true">
    <path d="M1 1 L7 5 L1 9Z"/>
  </svg>`;

export const ICON_GEAR =
  `<svg width="12" height="12" viewBox="0 0 16 16" fill="none"
       stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round"
       aria-hidden="true">
    <circle cx="8" cy="8" r="2.5"/>
    <path d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.42 1.42M11.53 11.53l1.42 1.42M3.05 12.95l1.42-1.42M11.53 4.47l1.42-1.42"/>
  </svg>`;

export const ICON_GEAR_SM =
  `<svg width="9" height="9" viewBox="0 0 16 16" fill="none"
       stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"
       aria-hidden="true">
    <circle cx="8" cy="8" r="2.5"/>
    <path d="M8 1v2M8 13v2M1 8h2M13 8h2M3.05 3.05l1.42 1.42M11.53 11.53l1.42 1.42M3.05 12.95l1.42-1.42M11.53 4.47l1.42-1.42"/>
  </svg>`;

export const ICON_POWER =
  `<svg width="12" height="12" viewBox="0 0 14 14" fill="none"
       stroke="currentColor" stroke-width="1.4" stroke-linecap="round"
       aria-hidden="true">
    <path d="M7 2v5"/>
    <path d="M4 4.2A5 5 0 1 0 10 4.2"/>
  </svg>`;

export const ICON_SOLO =
  `<svg width="12" height="12" viewBox="0 0 14 14" fill="none"
       stroke="currentColor" stroke-width="1.4" stroke-linecap="round"
       aria-hidden="true">
    <circle cx="7" cy="7" r="5"/>
    <circle cx="7" cy="7" r="1.5" fill="currentColor" stroke="none"/>
  </svg>`;

// M-memory toggle button for playlist rows (20×17 px container).
// Delta: 20-10=10 (even), 17-9=8 (even).
export const ICON_M =
  `<svg width="10" height="9" viewBox="0 0 12 11" fill="none"
       stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"
       aria-hidden="true">
    <path d="M1 10V2L6 6L11 2V10"/>
  </svg>`;
