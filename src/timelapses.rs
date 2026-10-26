use std::sync::Arc;
use std::time::Duration;

use bambuspa_api::Timelapse;
use tokio::sync::{Mutex, watch};

use crate::avi::Video;
use crate::p1s::Link;
use crate::p1s::ftp::Ftp;
use crate::settings::Settings;

const DIR: &str = "/timelapse";
const EVERY: Duration = Duration::from_secs(5 * 60);
const MAX_VIDEO: u64 = 256 << 20;
const MAX_THUMBNAIL: u64 = 4 << 20;
const LOW_WATER: f64 = 0.9;

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
    playing: Mutex<Option<(String, Arc<Video>)>>,
}

impl Library {
    pub fn new(link: Link, settings: Settings) -> Library {
        Library(Arc::new(Inner {
            link,
            settings,
            session: Mutex::new(()),
            list: watch::Sender::new(Vec::new()),
            playing: Mutex::new(None),
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

    pub async fn frame(&self, name: &str, i: usize) -> Result<(Arc<Video>, usize), Error> {
        self.known(name)?;
        let mut playing = self.0.playing.lock().await;
        let video = match &*playing {
            Some((current, video)) if current == name => video.clone(),
            _ => {
                *playing = None;
                let data = self
                    .connect()
                    .await?
                    .retrieve(&format!("{DIR}/{name}"), MAX_VIDEO)
                    .await?;
                let video = Arc::new(Video::parse(data).map_err(Error::Failed)?);
                *playing = Some((name.to_string(), video.clone()));
                video
            }
        };
        if i >= video.len() {
            return Err(Error::NotFound);
        }
        Ok((video, i))
    }

    pub async fn thumbnail(&self, name: &str) -> Result<Vec<u8>, Error> {
        self.known(name)?;
        Ok(self
            .connect()
            .await?
            .retrieve(&thumbnail_of(name), MAX_THUMBNAIL)
            .await?)
    }

    fn known(&self, name: &str) -> Result<(), Error> {
        if self.0.list.borrow().iter().any(|t| t.name == name) {
            Ok(())
        } else {
            Err(Error::NotFound)
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
        let (video, i) = lib.frame(name, 1).await.ok().unwrap();
        assert_eq!((video.len(), video.frame(i)), (2, Some(PIXEL_JPEG)));
        assert!(matches!(lib.frame(name, 2).await, Err(Error::NotFound)));
        assert_eq!(lib.thumbnail(name).await.ok().unwrap(), b"thumb");
        assert!(matches!(
            lib.thumbnail("video_2026-07-02_08-00-00.avi").await,
            Err(Error::NotFound)
        ));
    }
}
