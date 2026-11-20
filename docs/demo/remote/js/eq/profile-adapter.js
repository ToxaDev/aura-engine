// eq/profile-adapter.js — the ONE place the EQ window imports profile parsing
// and the AutoEq helpers. Both real modules live one level up and were
// committed on feat/eq (eq-profile.js = 15783de, eq-autoeq.js = 15783de).
export { parseProfile, exportApo, describe, MAX_BANDS } from '../eq-profile.js';
export { autoEqApi, searchAutoEq, autoEqCredit, AUTOEQ_SITE } from '../eq-autoeq.js';
