use std::sync::Arc;
use std::time::Duration;

use bambuspa_api::{Timelapse, TimelapseDownload};
use tokio::sync::{Mutex, watch};
use tokio::task::AbortHandle;

use crate::avi::Video;
use crate::p1s::Link;
use crate::p1s::ftp::Ftp;
use crate::settings::Settings;

const DIR: &str = "/timelapse";
const EVERY: Duration = Duration::from_secs(5 * 60);
const MAX_VIDEO: u64 = 256 << 20;
const MAX_THUMBNAIL: u64 = 4 << 20;
const LOW_WATER: f64 = 0.9;
const KEEP_DOWNLOADS: usize = 2;

pub enum Error {
    NotFound,
    Failed(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Error {
        Error::Failed(e.to_string())
    }
}

#[derive(Clone)]
pub struct Library(Arc<Inner>);

struct Inner {
    link: Link,
    settings: Settings,
    session: Mutex<()>,
    list: watch::Sender<Vec<Timelapse>>,
    downloads: std::sync::Mutex<Vec<Download>>,
}

#[derive(Clone)]
enum Fetch {
    Copying(u64),
    Ready(Arc<Video>),
    Failed(String),
}

/// A timelapse being copied, or copied, into a temporary file. The file has
/// no name on disk and the OS removes it once it is closed, which is when the
/// download is dropped (or the process exits).
struct Download {
    name: String,
    total: u64,
    state: watch::Receiver<Fetch>,
    task: AbortHandle,
}

impl Drop for Download {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Library {
    pub fn new(link: Link, settings: Settings) -> Library {
        Library(Arc::new(Inner {
            link,
            settings,
            session: Mutex::new(()),
            list: watch::Sender::new(Vec::new()),
            downloads: std::sync::Mutex::new(Vec::new()),
        }))
    }

    pub fn list(&self) -> watch::Receiver<Vec<Timelapse>> {
        self.0.list.subscribe()
    }

    pub async fn run(self) {
        let mut config = self.0.link.watch_config();
        loop {
            if config.borrow_and_update().complete()
                && let Err(err) = self.refresh().await
            {
                tracing::warn!("timelapses: {err}");
            }
            tokio::select! {
                _ = tokio::time::sleep(EVERY) => {}
                changed = config.changed() => if changed.is_err() { return },
            }
        }
    }

    pub async fn refresh(&self) -> std::io::Result<()> {
        let mut ftp = self.connect().await?;
        let mut videos: Vec<Timelapse> = ftp
            .list(DIR)
            .await?
            .into_iter()
            .filter(|e| !e.dir && is_video(&e.name))
            .map(|e| Timelapse {
                name: e.name,
                bytes: e.bytes as i64,
            })
            .collect();
        videos.sort_by(|a, b| a.name.cmp(&b.name));
        prune(
            &mut ftp,
            &mut videos,
            self.0.settings.values().timelapse_storage,
        )
        .await?;
        videos.reverse();
        self.0.list.send_if_modified(|current| {
            let changed = *current != videos;
            *current = videos;
            changed
        });
        Ok(())
    }

    /// Starts copying a timelapse off the SD card, unless it is already
    /// copied or on its way.
    pub fn download(&self, name: &str) -> Result<TimelapseDownload, Error> {
        let total = self.known(name)?;
        let mut downloads = self.downloads();
        let i = match downloads.iter().position(|d| d.name == name) {
            Some(i) if !matches!(*downloads[i].state.borrow(), Fetch::Failed(_)) => i,
            found => {
                if let Some(i) = found {
                    downloads.remove(i);
                }
                downloads.insert(0, self.start(name, total));
                downloads.truncate(KEEP_DOWNLOADS);
                0
            }
        };
        Ok(report(&downloads[i]))
    }

    /// How far the download of a timelapse has got.
    pub fn progress(&self, name: &str) -> Result<TimelapseDownload, Error> {
        self.known(name)?;
        let downloads = self.downloads();
        match downloads.iter().find(|d| d.name == name) {
            Some(d) => Ok(report(d)),
            None => Err(Error::NotFound),
        }
    }

    pub async fn frame(&self, name: &str, i: usize) -> Result<(Arc<Video>, Vec<u8>), Error> {
        let total = self.known(name)?;
        let mut state = {
            let mut downloads = self.downloads();
            match downloads.iter().find(|d| d.name == name) {
                Some(d) => d.state.clone(),
                None => {
                    let d = self.start(name, total);
                    let state = d.state.clone();
                    downloads.insert(0, d);
                    downloads.truncate(KEEP_DOWNLOADS);
                    state
                }
            }
        };
        let fetched = state
            .wait_for(|f| !matches!(f, Fetch::Copying(_)))
            .await
            .map_err(|_| Error::Failed("the download was cancelled".into()))?
            .clone();
        let video = match fetched {
            Fetch::Ready(video) => video,
            Fetch::Failed(err) => return Err(Error::Failed(err)),
            Fetch::Copying(_) => unreachable!(),
        };
        let reading = video.clone();
        let jpeg = tokio::task::spawn_blocking(move || reading.frame(i))
            .await
            .map_err(|e| Error::Failed(e.to_string()))??;
        match jpeg {
            Some(jpeg) => Ok((video, jpeg)),
            None => Err(Error::NotFound),
        }
    }

    fn downloads(&self) -> std::sync::MutexGuard<'_, Vec<Download>> {
        self.0.downloads.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn start(&self, name: &str, total: u64) -> Download {
        let (tx, state) = watch::channel(Fetch::Copying(0));
        let library = self.clone();
        let path = format!("{DIR}/{name}");
        let task = tokio::spawn(async move {
            let fetched = match library.fetch(&path, &tx).await {
                Ok(video) => Fetch::Ready(Arc::new(video)),
                Err(err) => {
                    tracing::warn!("timelapses: couldn't copy {path}: {err}");
                    Fetch::Failed(err)
                }
            };
            tx.send_replace(fetched);
        });
        Download {
            name: name.to_string(),
            total,
            state,
            task: task.abort_handle(),
        }
    }

    async fn fetch(&self, path: &str, progress: &watch::Sender<Fetch>) -> Result<Video, String> {
        let file = tempfile::tempfile().map_err(|e| format!("no temporary file: {e}"))?;
        let mut file = tokio::fs::File::from_std(file);
        self.connect()
            .await
            .map_err(|e| e.to_string())?
            .retrieve_into(path, MAX_VIDEO, &mut file, |n| {
                progress.send_replace(Fetch::Copying(n));
            })
            .await
            .map_err(|e| e.to_string())?;
        let file = file.into_std().await;
        tokio::task::spawn_blocking(move || Video::parse(file))
            .await
            .map_err(|e| e.to_string())?
    }

    pub async fn thumbnail(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.known(name)?;
        Ok(self
            .connect()
            .await?
            .retrieve(&thumbnail_of(name), MAX_THUMBNAIL)
            .await?)
    }

    fn known(&self, name: &str) -> Result<u64, Error> {
        match self.0.list.borrow().iter().find(|t| t.name == name) {
            Some(t) => Ok(t.bytes.max(0) as u64),
            None => Err(Error::NotFound),
        }
    }

    async fn connect(&self) -> std::io::Result<Session<'_>> {
        let guard = self.0.session.lock().await;
        let conf = self.0.link.config();
        let ftp = Ftp::connect(&conf.ip, self.0.link.ftp_port(), &conf.access_code).await?;
        Ok(Session { ftp, _guard: guard })
    }
}

struct Session<'a> {
    ftp: Ftp,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}

impl std::ops::Deref for Session<'_> {
    type Target = Ftp;
    fn deref(&self) -> &Ftp {
        &self.ftp
    }
}

impl std::ops::DerefMut for Session<'_> {
    fn deref_mut(&mut self) -> &mut Ftp {
        &mut self.ftp
    }
}

fn report(d: &Download) -> TimelapseDownload {
    let total = d.total as i64;
    match &*d.state.borrow() {
        Fetch::Copying(n) => TimelapseDownload {
            bytes: *n as i64,
            total,
            ..Default::default()
        },
        Fetch::Ready(_) => TimelapseDownload {
            bytes: total,
            total,
            ready: true,
            error: None,
        },
        Fetch::Failed(err) => TimelapseDownload {
            total,
            error: Some(err.clone()),
            ..Default::default()
        },
    }
}

fn is_video(name: &str) -> bool {
    name.starts_with("video_") && name.ends_with(".avi") && !name.contains('/')
}

fn thumbnail_of(name: &str) -> String {
    format!("{DIR}/thumbnail/{}.jpg", name.trim_end_matches(".avi"))
}

async fn prune(ftp: &mut Ftp, videos: &mut Vec<Timelapse>, cap: i64) -> std::io::Result<()> {
    let mut total: i64 = videos.iter().map(|v| v.bytes).sum();
    if cap <= 0 || total <= cap {
        return Ok(());
    }
    let target = (cap as f64 * LOW_WATER) as i64;
    while total > target && videos.len() > 1 {
        let oldest = videos.remove(0);
        ftp.delete(&format!("{DIR}/{}", oldest.name)).await?;
        if let Err(err) = ftp.delete(&thumbnail_of(&oldest.name)).await {
            tracing::info!(
                "timelapses: no thumbnail deleted for {}: {err}",
                oldest.name
            );
        }
        tracing::info!(
            "timelapses: deleted {} ({} MB) to stay under {} MB",
            oldest.name,
            oldest.bytes >> 20,
            cap >> 20
        );
        total -= oldest.bytes;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock;
    use crate::db::Db;
    use crate::p1s::{Config, Ports, StateCache};
    use crate::testing::printer::{FakePrinter, PIXEL_JPEG, avi};

    async fn library(cap_mb: i64) -> (Library, FakePrinter) {
        let printer = FakePrinter::start("127.0.0.1", 0, 0, 0, "S", "code").await;
        let db = Db::memory();
        let settings = Settings::new(db.clone()).unwrap();
        settings
            .set(crate::settings::TIMELAPSE_STORAGE, cap_mb)
            .unwrap();
        let ports = Ports {
            mqtt: printer.mqtt_port,
            camera: printer.camera_port,
            ftp: printer.ftp_port,
        };
        let log = crate::activity::Log::new(db, || 1 << 20, clock::system()).unwrap();
        let link = Link::new(StateCache::new(), log, clock::system(), ports);
        link.configure(Config {
            ip: "127.0.0.1".into(),
            serial: "S".into(),
            access_code: "code".into(),
        });
        (Library::new(link, settings), printer)
    }

    fn put(printer: &FakePrinter, day: u32, bytes: usize) -> String {
        let name = format!("video_2026-07-{day:02}_08-00-00.avi");
        printer.put_file(&format!("/timelapse/{name}"), vec![0; bytes]);
        printer.put_file(&thumbnail_of(&name), PIXEL_JPEG.to_vec());
        name
    }

    fn names(lib: &Library) -> Vec<String> {
        lib.list().borrow().iter().map(|t| t.name.clone()).collect()
    }

    #[tokio::test]
    async fn lists_only_videos_newest_first() {
        let (lib, printer) = library(0).await;
        let old = put(&printer, 1, 10);
        let new = put(&printer, 2, 10);
        printer.put_file("/timelapse/verify_job", vec![0; 16]);
        lib.refresh().await.unwrap();
        assert_eq!(names(&lib), [new, old]);
        assert_eq!(lib.list().borrow()[0].bytes, 10);
    }

    #[tokio::test]
    async fn over_the_cap_the_oldest_go_with_their_thumbnails() {
        let (lib, printer) = library(1).await;
        let mb = 1 << 20;
        let a = put(&printer, 1, mb / 2);
        let b = put(&printer, 2, mb / 2);
        let c = put(&printer, 3, mb / 4);
        let d = put(&printer, 4, mb / 4);
        lib.refresh().await.unwrap();
        assert_eq!(names(&lib), [d.clone(), c.clone()]);
        let files = printer.files();
        for gone in [&a, &b] {
            assert!(
                !files
                    .iter()
                    .any(|f| f.contains(gone.trim_end_matches(".avi"))),
                "{files:?}"
            );
        }
        assert!(files.contains(&thumbnail_of(&c)));
    }

    #[tokio::test]
    async fn the_newest_is_kept_whatever_its_size() {
        let (lib, printer) = library(1).await;
        put(&printer, 1, 1 << 20);
        let newest = put(&printer, 2, 2 << 20);
        lib.refresh().await.unwrap();
        assert_eq!(names(&lib), [newest]);
    }

    #[tokio::test]
    async fn without_a_cap_nothing_is_deleted() {
        let (lib, printer) = library(0).await;
        put(&printer, 1, 3 << 20);
        put(&printer, 2, 3 << 20);
        lib.refresh().await.unwrap();
        assert_eq!(names(&lib).len(), 2);
    }

    #[tokio::test]
    async fn frames_and_thumbnails_of_listed_videos_only() {
        let (lib, printer) = library(0).await;
        let name = "video_2026-07-01_08-00-00.avi";
        printer.put_file(
            &format!("/timelapse/{name}"),
            avi(&[PIXEL_JPEG, PIXEL_JPEG], 41_666),
        );
        printer.put_file(&thumbnail_of(name), b"thumb".to_vec());
        assert!(matches!(lib.frame(name, 0).await, Err(Error::NotFound)));
        lib.refresh().await.unwrap();
        let (video, jpeg) = lib.frame(name, 1).await.ok().unwrap();
        assert_eq!((video.len(), jpeg.as_slice()), (2, PIXEL_JPEG));
        assert!(matches!(lib.frame(name, 2).await, Err(Error::NotFound)));
        assert_eq!(lib.thumbnail(name).await.ok().unwrap(), b"thumb");
        assert!(matches!(
            lib.thumbnail("video_2026-07-02_08-00-00.avi").await,
            Err(Error::NotFound)
        ));
    }
}
