//! 图片落盘: 原子写 (tmp + rename), 文件名 `<站>_<流水号>_<时间>.jpg`。

use std::path::PathBuf;

use chrono::{Datelike, NaiveDateTime, Timelike};

pub struct ImageStore {
    dir: PathBuf,
    enabled: bool,
}

impl ImageStore {
    pub fn new(dir: PathBuf, enabled: bool) -> Self {
        Self { dir, enabled }
    }

    /// 保存图片, 返回文件名。
    pub fn save(&self, station: &str, serial: u16, ts: &NaiveDateTime, data: &[u8]) -> anyhow::Result<String> {
        if !self.enabled {
            anyhow::bail!("图片落盘已禁用");
        }
        std::fs::create_dir_all(&self.dir)?;
        let name = format!(
            "{station}_{serial:04}_{:04}{:02}{:02}_{:02}{:02}{:02}.jpg",
            ts.year(),
            ts.month(),
            ts.day(),
            ts.hour(),
            ts.minute(),
            ts.second()
        );
        let final_path = self.dir.join(&name);
        let tmp_path = self.dir.join(format!("{name}.tmp"));
        std::fs::write(&tmp_path, data)?;
        std::fs::rename(&tmp_path, &final_path)?;
        if data.len() >= 2 && !(data[0] == 0xFF && data[1] == 0xD8) {
            tracing::debug!(station, "图片数据非 JPG 魔数头, 仍按原样保存");
        }
        Ok(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_and_name() {
        let dir = tempfile::tempdir().unwrap();
        let store = ImageStore::new(dir.path().to_path_buf(), true);
        let ts = NaiveDateTime::parse_from_str("2026-09-09 08:00:00", "%Y-%m-%d %H:%M:%S").unwrap();
        let name = store.save("寨上站", 7, &ts, &[0xFF, 0xD8, 1, 2, 3]).unwrap();
        assert_eq!(name, "寨上站_0007_20260909_080000.jpg");
        assert!(dir.path().join(&name).is_file());
        let content = std::fs::read(dir.path().join(&name)).unwrap();
        assert_eq!(content, vec![0xFF, 0xD8, 1, 2, 3]);
    }
}
