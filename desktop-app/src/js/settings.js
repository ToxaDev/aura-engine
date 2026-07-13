import { state } from './state.js';
import { updateConvSpecsLine, updateFsDisplay } from './ui.js';

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

export function saveSettings() {
    const s = {
        convFs: document.getElementById('convFsSlider')?.value || '2',
        convTaps: getVal('convTapSlider'),
        convWindow: getVal('convWindow'),
        convCustomFilterPath: state.convCustomFilterPath,
        convCustomFilterName: state.convCustomFilterName,
        convCustomFilterTaps: state.convCustomFilterTaps,
        convApodizing: getVal('convApodizing') || '0',
        convHeadroom: getVal('convHeadroom') || '0',
        convAdaptiveApodizer: getCheck('convAdaptiveApodizer'),
        convHybridPhase: getCheck('convHybridPhase'),
        convGpuCheck: getCheck('convGpuCheck'),
        convFirResampling: getCheck('convFirResampling')
    };
    localStorage.setItem('auraSettings', JSON.stringify(s));
}

export function loadSettings() {
    const data = localStorage.getItem('auraSettings');
    if (!data) return;
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
        if (s.convTaps !== undefined) {
            let t = parseInt(s.convTaps);
            if (t > 3) t = 3;
            setVal('convTapSlider', t);
        }
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
        updateFsDisplay();
    } catch(e) {}
}

export function applySettingsDependencies() {
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
