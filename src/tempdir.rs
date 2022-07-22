// GNU AGPL v3 License

use anyhow::Result;
use std::{
    env,
    path::{Path, PathBuf},
};
use tokio::fs;

pub(crate) struct TempDir {
    path: PathBuf,
    deleted: bool,
}

impl TempDir {
    pub(crate) async fn new() -> Result<Self> {
        // generate a radnom number
        let random_number = tokio::task::spawn_blocking(|| {
            let mut number = 0u64;
            let number_bytes = bytemuck::bytes_of_mut(&mut number);
            getrandom::getrandom(number_bytes)?;
            anyhow::Ok(number)
        });

        // create a temporary directory
        let temp_dir_root = env::temp_dir();

        // join the path in
        let temp_dir = temp_dir_root.join(format!("carcetti-{}", random_number.await??));

        // create the dir
        fs::create_dir_all(&temp_dir).await?;

        Ok(Self {
            path: temp_dir,
            deleted: false,
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    /// Deletes the temporary directory.
    pub(crate) async fn delete(mut self) -> Result<()> {
        self.deleted = true;
        fs::remove_dir_all(self.path()).await?;
        Ok(())
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.deleted {
            // spawn a task to delete it manually
            let path = self.path().to_path_buf();
            let _ = tokio::task::spawn_blocking(|| {
                std::fs::remove_dir_all(path).ok();
            });
        }
    }
}
