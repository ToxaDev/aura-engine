//! Which filter files a setting is missing: what a conversion would pick
//! for each cell (the lookup itself, [`flt::find_blob_in`]), checked against
//! the signed list.

use std::path::{Path, PathBuf};

use super::manifest::{Entry, Manifest};
use crate::audio::converter::dsp::filter as flt;

/// One cell of the filter matrix: a tap count at a design rate in one phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cell {
    pub taps: usize,
    pub rate: u32,
    pub minimum: bool,
}

/// The cells a setting needs: `taps` at `fs` in each phase asked for, for
/// each source family (44.1 / 48 kHz bases). A hi-res source takes the cell
/// of its family at the same factor ([`flt::design_rate`]), so the families
/// are all a playlist adds. Cells the ladder does not offer are left out.
pub fn cells(taps: usize, fs: u32, minimum: &[bool], bases: &[u32]) -> Vec<Cell> {
    let mut out = Vec::new();
    for &base in bases {
        let rate = base * fs;
        if !flt::TARGET_RATES.contains(&rate) || !flt::offered_at(taps, rate) {
            continue;
        }
        for &m in minimum {
            let c = Cell { taps, rate, minimum: m };
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    out
}

/// The cells hi-res sources add at `fs`: a source at base × m played at
/// base × `fs` takes the cell at base × `fs`/m ([`flt::design_rate`]), for
/// m = 2, 4, 8, 16 while the output is still at least twice the source.
/// Cells below the rung's lowest factor ([`flt::min_fs`]) are left out.
pub fn hires_cells(taps: usize, fs: u32, minimum: &[bool], bases: &[u32]) -> Vec<Cell> {
    let mut out = Vec::new();
    for &base in bases {
        for m in [2, 4, 8, 16] {
            if fs % m != 0 || fs / m < 2 {
                continue;
            }
            let rate = flt::design_rate(base * m, base * fs);
            if !flt::TARGET_RATES.contains(&rate) || !flt::offered_at(taps, rate) {
                continue;
            }
            for &mm in minimum {
                let c = Cell { taps, rate, minimum: mm };
                if !out.contains(&c) {
                    out.push(c);
                }
            }
        }
    }
    out
}

/// A file to download.
#[derive(Clone, Debug, PartialEq)]
pub struct Need {
    pub name: String,
    pub size: u64,
    /// What an interrupted download already left in `<name>.part`.
    pub part_bytes: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    pub files: Vec<Need>,
    /// Cells the release has no files for.
    pub not_offered: Vec<Cell>,
    /// Broken copies the lookup finds before the download folder: the
    /// downloaded files would not be used while these are there.
    pub shadowed: Vec<PathBuf>,
}

impl Plan {
    /// Bytes still to come over the network.
    pub fn to_fetch(&self) -> u64 {
        self.files.iter().map(|n| n.size - n.part_bytes).sum()
    }
}

fn size_is(path: &Path, size: u64) -> bool {
    std::fs::metadata(path).map(|m| m.is_file() && m.len() == size).unwrap_or(false)
}

/// What `cells` miss, given the lookup's folders `dirs` (in its order) and
/// the folder downloads go to, `dest`.
///
/// A cell is fine when the file the lookup picks is not one of the release's
/// (a legacy or self-made filter: what the user has is what plays), or when
/// every file of the cell sits next to it at its listed size. Otherwise the
/// cell's files that `dest` does not already hold at their size are needed —
/// all of them, not only the broken ones elsewhere, because the lookup takes
/// a cell from one folder.
pub fn plan(manifest: &Manifest, cells: &[Cell], dirs: &[PathBuf], dest: &Path) -> Plan {
    let mut out = Plan::default();
    for c in cells {
        let entries: Vec<&Entry> = manifest.cell(c.taps, c.rate, c.minimum);
        if entries.is_empty() {
            out.not_offered.push(*c);
            continue;
        }
        let phase = if c.minimum { "minimum_phase" } else { "linear_phase" };
        if let Some(found) = flt::find_blob_in(dirs, c.taps, c.rate, phase) {
            let name = found.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let at = found.parent().unwrap_or(Path::new(""));
            if manifest.get(&name).is_none() || entries.iter().all(|e| size_is(&at.join(&e.name), e.size)) {
                continue;
            }
            // A broken copy the lookup reaches before `dest` keeps winning.
            let dest_rank = dirs.iter().position(|d| d == dest);
            let found_rank = dirs.iter().position(|d| d == at);
            if at != dest && matches!((found_rank, dest_rank), (Some(f), Some(d)) if f < d) {
                out.shadowed.push(found.clone());
            }
        }
        for e in entries {
            let path = dest.join(&e.name);
            if size_is(&path, e.size) || out.files.iter().any(|n| n.name == e.name) {
                continue;
            }
            let part = std::fs::metadata(dest.join(format!("{}.part", e.name)))
                .map(|m| m.len())
                .unwrap_or(0);
            out.files.push(Need {
                name: e.name.clone(),
                size: e.size,
                part_bytes: if part <= e.size { part } else { 0 },
            });
        }
    }
    out
}

/// The free space a download of `bytes` asks for: the bytes and a margin,
/// 2 % of them but at least 256 MB, so the disk is not filled to the brim.
pub fn space_needed(bytes: u64) -> u64 {
    bytes + (bytes / 50).max(256 << 20)
}

#[cfg(test)]
mod tests {
    use super::super::manifest::tests::{entry_json, manifest_json, SHA_A};
    use super::super::manifest::parse;
    use super::*;

    const B0: &str = "fir_1G_705600_linear_phase.b0.npy";
    const ENDS: &str = "fir_1G_705600_linear_phase.b0.ends.npy";
    const META: &str = "fir_1G_705600_linear_phase.b0.json";

    fn manifest() -> Manifest {
        parse(&manifest_json(vec![
            entry_json(B0, 100, SHA_A, "1G", 705_600, "linear"),
            entry_json(ENDS, 20, SHA_A, "1G", 705_600, "linear"),
            entry_json(META, 3, SHA_A, "1G", 705_600, "linear"),
            entry_json("fir_30M_352800_linear_phase.npy", 50, SHA_A, "30M", 352_800, "linear"),
        ]))
        .unwrap()
    }

    fn put(dir: &Path, name: &str, len: usize) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(name), vec![0u8; len]).unwrap();
    }

    const CELL_1G: Cell = Cell { taps: 1_000_000_000, rate: 705_600, minimum: false };
    const CELL_30M: Cell = Cell { taps: 30_000_000, rate: 352_800, minimum: false };

    fn names(p: &Plan) -> Vec<&str> {
        p.files.iter().map(|n| n.name.as_str()).collect()
    }

    #[test]
    fn cells_cover_phases_and_families_where_offered() {
        let c = cells(30_000_000, 8, &[false, true], &[44_100, 48_000]);
        assert_eq!(c.len(), 4);
        assert!(c.contains(&Cell { taps: 30_000_000, rate: 384_000, minimum: true }));
        // 1G starts at FS8; FS4 has nothing.
        assert!(cells(1_000_000_000, 4, &[false], &[44_100]).is_empty());
        assert_eq!(cells(5_000, 2, &[false, false], &[44_100, 44_100]).len(), 1);
    }

    #[test]
    fn hires_sources_add_the_lower_factor_cells_the_rung_offers() {
        let rates = |c: Vec<Cell>| c.iter().map(|c| c.rate).collect::<Vec<_>>();
        // 30M at FS16: 88.2k → ×8, 176.4k → ×4, 352.8k → ×2; 705.6k would be ×1.
        assert_eq!(rates(hires_cells(30_000_000, 16, &[false], &[44_100])), vec![352_800, 176_400, 88_200]);
        // 1G starts at FS8: only 88.2k/96k at FS16 qualify.
        assert_eq!(rates(hires_cells(1_000_000_000, 16, &[false], &[44_100, 48_000])), vec![352_800, 384_000]);
        assert!(hires_cells(1_000_000_000, 8, &[false], &[44_100]).is_empty());
        assert!(hires_cells(5_000, 2, &[false], &[44_100]).is_empty());
        assert_eq!(hires_cells(5_000, 4, &[false, true], &[48_000]).len(), 2);
    }

    #[test]
    fn an_empty_install_needs_every_file_of_its_cells() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out");
        let p = plan(&manifest(), &[CELL_1G, CELL_30M], &[dest.clone()], &dest);
        assert_eq!(names(&p).len(), 4);
        assert_eq!(p.to_fetch(), 173);
        let missing = Cell { taps: 5_000, rate: 88_200, minimum: true };
        assert_eq!(plan(&manifest(), &[missing], &[dest.clone()], &dest).not_offered, vec![missing]);
    }

    #[test]
    fn a_whole_cell_at_its_sizes_needs_nothing_and_a_wrong_size_is_fetched_again() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out");
        put(&dest, B0, 100);
        put(&dest, ENDS, 20);
        put(&dest, META, 3);
        assert!(plan(&manifest(), &[CELL_1G], &[dest.clone()], &dest).files.is_empty());
        put(&dest, ENDS, 19);
        assert_eq!(names(&plan(&manifest(), &[CELL_1G], &[dest.clone()], &dest)), vec![ENDS]);
        std::fs::remove_file(dest.join(META)).unwrap();
        put(&dest, ENDS, 20);
        assert_eq!(names(&plan(&manifest(), &[CELL_1G], &[dest.clone()], &dest)), vec![META]);
    }

    #[test]
    fn a_filter_the_release_does_not_list_is_left_alone() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out");
        // A self-made whole 1G filter: the lookup takes it before branch 0.
        put(&dest, "fir_1G_705600_linear_phase.npy", 5);
        assert!(plan(&manifest(), &[CELL_1G], &[dest.clone()], &dest).files.is_empty());
        // A legacy 30M at FS8 serves 352.8 kHz.
        put(&dest, "fir_30M_linear_phase.npy", 5);
        assert!(plan(&manifest(), &[CELL_30M], &[dest.clone()], &dest).files.is_empty());
    }

    #[test]
    fn a_broken_cell_elsewhere_is_fetched_whole_and_a_part_is_counted() {
        let t = tempfile::tempdir().unwrap();
        let dest = t.path().join("out");
        let later = t.path().join("repo");
        let earlier = t.path().join("env");
        put(&later, B0, 100);
        put(&later, ENDS, 20);
        put(&dest, META, 3);
        put(&dest, &format!("{B0}.part"), 40);
        let p = plan(&manifest(), &[CELL_1G], &[dest.clone(), later.clone()], &dest);
        assert_eq!(names(&p), vec![ENDS, B0]);
        assert_eq!(p.files[1].part_bytes, 40);
        assert_eq!(p.to_fetch(), 80);
        assert!(p.shadowed.is_empty());

        // The same broken copy in a folder searched before `dest` wins over the download.
        let p = plan(&manifest(), &[CELL_1G], &[earlier.clone(), dest.clone()], &dest);
        assert!(p.shadowed.is_empty(), "nothing in the earlier folder yet");
        put(&earlier, B0, 100);
        let p = plan(&manifest(), &[CELL_1G], &[earlier.clone(), dest.clone()], &dest);
        assert_eq!(p.shadowed, vec![earlier.join(B0)]);

        // A part longer than the file is not counted.
        put(&dest, &format!("{ENDS}.part"), 21);
        let p = plan(&manifest(), &[CELL_1G], &[dest.clone()], &dest);
        assert_eq!(p.files.iter().find(|n| n.name == ENDS).unwrap().part_bytes, 0);
    }

    #[test]
    fn the_margin_is_two_percent_but_at_least_256_mb() {
        assert_eq!(space_needed(0), 256 << 20);
        assert_eq!(space_needed(100 << 30), (100 << 30) + (2 << 30));
    }
}
