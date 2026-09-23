import { state } from './state.js';
import { getXtcGeometry, setXtcGeometry } from './xtc.js';
import { updateConvSpecsLine, updateFsDisplay } from './ui.js';
import { TAP_PRESETS, tapIndexFromSaved } from './inventory.js';

function getVal(id) {
    const el = document.getElementById(id);
    return el ? el.value : null;
}
function setVal(id, val) {
    const el = document.getElementById(id);
    if (el) el.value = val;
}
function getCheck(id) {
    const el = document.getElementById(id);
    return el ? el.checked : null;
}
function setCheck(id, val) {
    const el = document.getElementById(id);
    if (el) el.checked = val;
}

/// Written into every blob from 1.3.4 on. A blob without it was saved by 1.3.3
/// or earlier, and in those the Polyphase FIR box says nothing about anybody's
/// choice: it shipped unticked until 1.3.0, and its "off" was then carried
/// through every save that followed. Only a blob that carries this number can
/// be trusted to mean what its PFR field says.
const SETTINGS_REV = 2;

export function saveSettings() {
    const s = {
        settingsRev: SETTINGS_REV,
        convFs: document.getElementById('convFsSlider')?.value || '2',
        convTapCount: TAP_PRESETS[Math.min(parseInt(getVal('convTapSlider')) || 0, TAP_PRESETS.length - 1)],
        convWindow: getVal('convWindow'),
        convCustomFilterPath: state.convCustomFilterPath,
        convCustomFilterName: state.convCustomFilterName,
        convCustomFilterTaps: state.convCustomFilterTaps,
        convApodizing: getVal('convApodizing') || '0',
        convHeadroom: getVal('convHeadroom') || '0',
        convAdaptiveApodizer: getCheck('convAdaptiveApodizer'),
        convHybridPhase: getCheck('convHybridPhase'),
        convGpuCheck: getCheck('convGpuCheck'),
        convFirResampling: getCheck('convFirResampling'),
        convSubsonic: getCheck('convSubsonic'),
        convSubsonicHz: getVal('convSubsonicHz'),
        labFeatures: {
            declip: getCheck('convLabDeclip') || false,
            isp: getCheck('convLabIsp') || false,
            tfsPhase: getCheck('convLabTfs') || false,
            continuousAlpha: getCheck('convLabAlpha') || false,
            adaptiveHeadroom: getCheck('convLabHeadroom') || false,
            xtc: getCheck('convLabXtc') || false,
        },
        // Millimetres, not a checkbox: the one setting that describes the
        // room rather than the file. Kept beside labFeatures rather than
        // inside it so a future stage can read it without owning XTC.
        xtcGeometry: getXtcGeometry(),
    };
    localStorage.setItem('auraSettings', JSON.stringify(s));
}

/// Restore the previous session's settings.
///
/// Returns whether there was anything to restore — a first run has to be told
/// apart from a returning one, because only a first run may have its filter
/// settings chosen for it.
export function loadSettings() {
    const data = localStorage.getItem('auraSettings');
    if (!data) return false;
    try {
        const s = JSON.parse(data);
        if (s.convFs !== undefined) {
            setVal('convFsSlider', s.convFs);
        } else if (s.convRate) {
            // Migrate legacy convRate to nearest FS
            const legacyRateToFs = { '88200': '0', '88000': '0', '176400': '1', '192000': '1', '352800': '2', '384000': '2', '705600': '3', '768000': '3' };
            const fsIdx = legacyRateToFs[s.convRate] || '2';
            setVal('convFsSlider', fsIdx);
        }
        const tapIndex = tapIndexFromSaved(s);
        if (tapIndex !== null) setVal('convTapSlider', tapIndex);
        if (s.convWindow) setVal('convWindow', s.convWindow);
        if (s.convCustomFilterPath) {
            state.convCustomFilterPath = s.convCustomFilterPath;
            state.convCustomFilterName = s.convCustomFilterName || '';
            state.convCustomFilterTaps = s.convCustomFilterTaps || 0;
        }
        if (s.convApodizing) setVal('convApodizing', s.convApodizing);
        if (s.convHeadroom) setVal('convHeadroom', s.convHeadroom);
        if (s.convAdaptiveApodizer !== undefined) setCheck('convAdaptiveApodizer', s.convAdaptiveApodizer);
        if (s.convHybridPhase !== undefined) setCheck('convHybridPhase', s.convHybridPhase);
        if (s.convGpuCheck !== undefined) setCheck('convGpuCheck', s.convGpuCheck);
        if (s.convFirResampling !== undefined) setCheck('convFirResampling', s.convFirResampling);
        // Saved before 1.3.4: an "off" here is most likely the pre-1.3.0 default
        // carried forward, not a choice, and it kept the file on the standard
        // route — where TFS stands down and a long file has no segmented pass
        // and is refused for RAM. The markup ships it on; so does this, once.
        if ((s.settingsRev ?? 0) < SETTINGS_REV) setCheck('convFirResampling', true);
        // Absent from settings saved before 1.2.8: the filter keeps what the
        // markup ships it as, which since 1.3.0 is on, at 15 Hz.
        if (s.convSubsonic !== undefined) setCheck('convSubsonic', s.convSubsonic);
        if (s.convSubsonicHz) setVal('convSubsonicHz', s.convSubsonicHz);
        if (s.labFeatures) {
            setCheck('convLabDeclip', s.labFeatures.declip || false);
            setCheck('convLabIsp', s.labFeatures.isp || false);
            setCheck('convLabTfs', s.labFeatures.tfsPhase || false);
            setCheck('convLabAlpha', s.labFeatures.continuousAlpha || false);
            setCheck('convLabHeadroom', s.labFeatures.adaptiveHeadroom || false);
            setCheck('convLabXtc', s.labFeatures.xtc || false);
        }
        // A blob saved before 1.3.0 says nothing about the stages that release
        // added, and they keep what the markup ships them as: on. That is the
        // point for Declip, ISP and Adaptive Headroom. Not for TFS, which takes
        // the slot Hybrid-Phase occupies: a 1.2.x user who had Hybrid-Phase on
        // came up with both ticked, the rack flagged the pair as broken without
        // settling it, and the engine settled it by running TFS in place of
        // Hybrid-Phase — files converted in a mode nobody had chosen.
        //
        // Checked on every blob, not only the old ones: the first change made
        // in 1.3.0–1.3.3 saved that pair as it stood, labFeatures and all. The
        // rack never leaves both on by itself — ticking either clears the
        // other — so both on can only be that inheritance, and the choice that
        // was actually made wins.
        if (s.convHybridPhase && getCheck('convLabTfs')) {
            setCheck('convLabTfs', false);
        }
        // Restored before the rack is painted, so the XTC badge knows on the
        // first frame whether it is holding a triangle or asking for one.
        if (s.xtcGeometry) setXtcGeometry(s.xtcGeometry);
        updateFsDisplay();
        return true;
    } catch(e) {
        // A corrupt blob is indistinguishable from a first run for our
        // purposes: whatever survived is arbitrary, so let the installed
        // filters decide the opening state.
        return false;
    }
}

export function applySettingsDependencies() {
    // Restoring settings writes .checked directly, which fires no event; the
    // badges would otherwise keep showing the previous state.
    window.__dspSyncRack?.();
    const aaChecked = getCheck('convAdaptiveApodizer');
    const hpChecked = getCheck('convHybridPhase');
    const apodSel = document.getElementById('convApodizing');
    const winSel = document.getElementById('convWindow');

    if (apodSel) {
        if (aaChecked) {
            apodSel.value = '0';
            apodSel.disabled = true;
            apodSel.style.opacity = '0.4';
        } else {
            apodSel.disabled = false;
            apodSel.style.opacity = '1';
        }
    }

    if (winSel) {
        if (hpChecked) {
            winSel.value = '4';
            winSel.disabled = true;
            winSel.style.opacity = '0.4';
        } else {
            winSel.disabled = false;
            winSel.style.opacity = '1';
        }
    }

    // Lock/unlock the custom filter load button based on Hybrid-Phase state
    const loadBtn = document.getElementById('convLoadFilterBtn');
    if (loadBtn) {
        if (hpChecked) {
            loadBtn.disabled = true;
            loadBtn.style.opacity = '0.35';
            loadBtn.style.cursor = 'not-allowed';
        } else {
            loadBtn.disabled = false;
            loadBtn.style.opacity = '1';
            loadBtn.style.cursor = '';
        }
    }

    const gpuChecked = getCheck('convGpuCheck');
    const gpuBlock = document.getElementById('convGpuBlock');
    const gpuStrip = document.getElementById('convGpuStrip');
    const gpuText = document.getElementById('convGpuText');
    const gpuBadge = document.getElementById('convGpuBadge');

    if (gpuBlock && gpuStrip && gpuText && gpuBadge) {
        if (gpuChecked) {
            gpuBlock.style.border = '1px solid rgba(56, 189, 248, 0.4)';
            gpuBlock.style.background = 'linear-gradient(90deg, rgba(15,23,42,0.8) 0%, rgba(30,58,138,0.4) 50%, rgba(15,23,42,0.8) 100%), repeating-linear-gradient(45deg, transparent, transparent 10px, rgba(56,189,248,0.05) 10px, rgba(56,189,248,0.05) 20px)';
            gpuBlock.style.boxShadow = 'inset 0 0 15px rgba(56,189,248,0.1)';
            gpuStrip.style.background = '#38bdf8';
            gpuStrip.style.boxShadow = '0 0 8px #38bdf8';
            gpuText.style.color = '#38bdf8';
            gpuText.style.textShadow = '0 0 6px rgba(56,189,248,0.6)';
            gpuBadge.style.color = '#7dd3fc';
            gpuBadge.style.border = '1px solid rgba(56,189,248,0.3)';
            gpuBadge.style.background = 'rgba(56,189,248,0.1)';
        } else {
            gpuBlock.style.border = '1px solid rgba(255, 255, 255, 0.1)';
            gpuBlock.style.background = 'rgba(15, 23, 42, 0.8)';
            gpuBlock.style.boxShadow = 'none';
            gpuStrip.style.background = 'rgba(255, 255, 255, 0.2)';
            gpuStrip.style.boxShadow = 'none';
            gpuText.style.color = 'rgba(255, 255, 255, 0.5)';
            gpuText.style.textShadow = 'none';
            gpuBadge.style.color = 'rgba(255, 255, 255, 0.3)';
            gpuBadge.style.border = '1px solid rgba(255, 255, 255, 0.1)';
            gpuBadge.style.background = 'transparent';
        }
    }
}
