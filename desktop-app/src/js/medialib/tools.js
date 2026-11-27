// ══════════════════════════════════════════════════════════════════════
// The landscape library's own pages (model.js TOOLS): each is a function of
// the browser's context — { toast(text), menu(at, items), status(), layout }
// — that returns the page's node (or a promise of it). The node may carry
// `__after()` (called once it is in the document), `__refresh()` (the
// browser asks for fresh data; the page is not built again) and `__leave()`
// (called before another page takes its place: timers, frames and listeners
// end there).
// ══════════════════════════════════════════════════════════════════════

import { conversionsPage } from './conversions.js';
import { visualizerPage } from './visualizer.js';
import { analyzerPage } from './analyzer.js';
import { dupesPage } from './dupes.js';

export const TOOL_PAGES = {
    conversions: conversionsPage,
    dupes: dupesPage,
    analyzer: analyzerPage,
    visualizer: visualizerPage,
};
