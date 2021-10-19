use async_trait::async_trait;
use chrono::{DateTime, FixedOffset};
use google_youtube3::{
    api::Scope,
    api::{PlaylistItem, PlaylistItemListResponse, PlaylistItemSnippet, ResourceId},
    client::Result,
    YouTube,
};
use hyper::Response;
use std::{cmp::Ordering, fmt};

#[derive(Default, Clone, PartialEq, Debug)]
pub struct Item {
    pub video_id: String,
    playlist_item_id: String,
    pub title: String,
    pub scheduled_start_time: Option<DateTime<FixedOffset>>,
    pub actual_start_time: Option<DateTime<FixedOffset>>,
    pub blocked: bool,
}

impl fmt::Display for Item {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "({}: {})", self.video_id, self.title)
    }
}

#[async_trait]
pub trait Playlist {
    /// items returns a vector of the items in the playlist.
    async fn items(self: &Self) -> Result<Vec<Item>>;

    /// sort orders the playlist as follows:
    /// * streamed videos in reverse chronological order (newest first), followed
    /// * not-yet-streamed videos again in reverse chronological order (newest first), followed by
    /// * videos for which there is no time information.
    async fn sort(self: &Self) -> Result<()>;

    /// prune removes any invalid videos from the playlist. These include:
    /// * deleted videos
    /// * videos for which there is no time information (e.g. with no live streaming information such as scheduled start time).
    async fn prune(self: &Self, max_streamed: usize) -> Result<()>;

    // print prints the playlist to standard error.
    async fn print(self: &Self) -> Result<()>;
}

struct PlaylistImpl {
    hub: YouTube,
    id: String,
    dry_run: bool,
    debug: bool,
}

/// new constructs a Playlist trait implementation for manipulating the playlist with the given playlist id.
/// If dry-run is true, information will be printed out but the playlist will not be updated on YouTube.
/// Debugging information is printed if and only if debug is true.
pub fn new(hub: YouTube, id: &str, dry_run: bool, debug: bool) -> impl Playlist {
    PlaylistImpl {
        hub: hub,
        id: id.to_owned(),
        dry_run: dry_run,
        debug: debug,
    }
}

#[async_trait]
impl Playlist for PlaylistImpl {
    async fn items(self: &PlaylistImpl) -> Result<Vec<Item>> {
        let mut list: Vec<Item> = vec![];

        let (_, mut res) = playlist_items(&self.hub, &self.id, &None).await?;
        while let Some(items) = &res.items {
            for item in items {
                let video_id = item
                    .content_details
                    .as_ref()
                    .unwrap()
                    .video_id
                    .as_ref()
                    .unwrap();

                let (_, v) = self
                    .hub
                    .videos()
                    .list(&vec![
                        "liveStreamingDetails".into(),
                        "contentDetails".into(),
                    ])
                    .add_id(video_id)
                    .doit()
                    .await?;

                let mut it = Item {
                    video_id: video_id.to_owned(),
                    playlist_item_id: item.id.as_ref().unwrap().to_owned(),
                    title: item
                        .snippet
                        .as_ref()
                        .unwrap()
                        .title
                        .as_ref()
                        .unwrap()
                        .to_owned(),
                    ..Default::default()
                };

                let videos = v.items.unwrap();

                if videos.len() > 0 {
                    let live_streaming_details =
                        videos.get(0).unwrap().live_streaming_details.as_ref();
                    if let Some(details) = live_streaming_details {
                        it.scheduled_start_time = details
                            .scheduled_start_time
                            .as_ref()
                            .map(|d| DateTime::parse_from_rfc3339(&d).unwrap());
                        it.actual_start_time = details
                            .actual_start_time
                            .as_ref()
                            .map(|d| DateTime::parse_from_rfc3339(&d).unwrap());
                    }
                    if let Some(content_details) = videos.get(0).unwrap().content_details.as_ref() {
                        if let Some(restriction) = content_details.region_restriction.as_ref() {
                            if let Some(blocked) = restriction.blocked.as_ref() {
                                it.blocked = !blocked.is_empty();
                            }
                        }
                    }
                }
                list.push(it)
            }
            if res.next_page_token.is_some() {
                res = playlist_items(&self.hub, &self.id, &res.next_page_token)
                    .await?
                    .1;
            } else {
                res.items = None;
            }
        }

        if self.debug {
            eprintln!("playlist items: {:?}", list);
        }
        Ok(list)
    }

    async fn sort(self: &Self) -> Result<()> {
        let mut items = self.items().await?;
        let original_items = items.clone();
        sort_items(&mut items);
        if items == original_items {
            eprintln!("Playlist is already in the correct order");
            Ok(())
        } else {
            if self.dry_run {
                eprintln!("Playlist would be sorted into this order:");
                print(items)?;
                eprintln!("");
            } else {
                // Re-order the playlist to match the sorted items.
                for (n, item) in items.iter().enumerate() {
                    self.hub
                        .playlist_items()
                        .update(PlaylistItem {
                            id: Some(item.playlist_item_id.clone()),
                            snippet: Some(PlaylistItemSnippet {
                                playlist_id: Some(self.id.clone()),
                                resource_id: Some(ResourceId {
                                    kind: Some("youtube#video".to_owned()),
                                    video_id: Some(item.video_id.clone()),
                                    ..Default::default()
                                }),
                                position: Some(n as u32),
                                ..Default::default()
                            }),
                            ..Default::default()
                        })
                        .add_scope(Scope::Full)
                        .doit()
                        .await?;
                }
            }
            Ok(())
        }
    }

    async fn prune(self: &Self, max_streamed: usize) -> Result<()> {
        // Remove surplus streamed videos and invalid videos from the playlist
        self.sort().await?;
        let mut n = 0;
        for i in self.items().await? {
            if i.blocked {
                if !self.dry_run {
                    eprintln!("Deleting playlist item for blocked video {}", i);
                    prune_item(&self.hub, i.playlist_item_id).await?;
                } else {
                    eprintln!(
                        "Non-dry run would delete playlist item for blocked video {}",
                        i
                    );
                }
            } else if i.actual_start_time.is_some() {
                n += 1;
                if n > max_streamed {
                    if !self.dry_run {
                        eprintln!("Removing surplus streamed video from playlist {}", i);
                        prune_item(&self.hub, i.playlist_item_id).await?;
                    } else {
                        eprintln!(
                            "Non-dry run would remove surplus streamed video from playlist {}",
                            i
                        );
                    }
                }
            } else if i.scheduled_start_time.is_none() {
                if !self.dry_run {
                    eprintln!("Deleting playlist item for unscheduled video {}", i);
                    prune_item(&self.hub, i.playlist_item_id).await?;
                } else {
                    eprintln!(
                        "Non-dry run would delete playlist item for unscheduled video {}",
                        i
                    );
                }
            }
        }
        Ok(())
    }

    async fn print(self: &Self) -> Result<()> {
        print(self.items().await?)
    }
}

fn print(items: Vec<Item>) -> Result<()> {
    for video in items {
        eprintln!(
            "{}: {} {:?} {:?} {} {}",
            video.video_id,
            video.title,
            video.scheduled_start_time,
            video.actual_start_time,
            if video.scheduled_start_time.is_none() {
                "** invalid"
            } else {
                ""
            },
            if video.blocked { "** blocked" } else { "" }
        );
    }
    Ok(())
}

async fn prune_item(hub: &YouTube, playlist_item_id: String) -> Result<()> {
    hub.playlist_items()
        .delete(&playlist_item_id)
        .add_scope(Scope::Full)
        .doit()
        .await?;
    Ok(())
}

async fn playlist_items(
    hub: &YouTube,
    playlist_id: &str,
    next_page_token: &Option<String>,
) -> Result<(Response<hyper::body::Body>, PlaylistItemListResponse)> {
    let mut req = hub
        .playlist_items()
        .list(&vec![
            "snippet".into(),
            "id".into(),
            "contentDetails".into(),
        ])
        .playlist_id(playlist_id);
    if let Some(next) = next_page_token {
        req = req.page_token(&next);
    }
    req.doit().await
}

fn sort_items(items: &mut Vec<Item>) {
    items.sort_by(|v, w| {
        // println!("v: {:?}\nw: {:?}", v, w);
        if v.actual_start_time.is_some() {
            if w.actual_start_time.is_some() {
                // Order streamed items in reverse chronological order
                v.actual_start_time
                    .unwrap()
                    .cmp(&w.actual_start_time.unwrap())
                    .reverse()
            } else {
                // Order streamed items before unstreamed items
                Ordering::Less
            }
        } else if w.actual_start_time.is_some() {
            // Order streamed items before unstreamed items
            Ordering::Greater
        } else if v.scheduled_start_time.is_some() {
            if w.scheduled_start_time.is_some() {
                // Order unstreamed, scheduled items in reverse chronological order
                v.scheduled_start_time
                    .unwrap()
                    .cmp(&w.scheduled_start_time.unwrap())
                    .reverse()
            } else {
                // Order unstreamed, scheduled items before unstreamed, unscheduled items
                Ordering::Less
            }
        } else if w.scheduled_start_time.is_some() {
            // Order unstreamed, scheduled items before unstreamed, unscheduled items
            Ordering::Greater
        } else {
            // Leave the order of unstreamed, unscheduled items alone
            Ordering::Equal
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sort_items_empty() {
        let mut v = vec![];
        sort_items(&mut v);
        assert_eq!(v, vec![]);
    }

    #[test]
    fn sort_items_unstreamed_scheduled() {
        let mut v = vec![new_scheduled_item(1), new_scheduled_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v2", "v1"]);

        v = vec![new_scheduled_item(2), new_scheduled_item(1)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v2", "v1"]);
    }

    #[test]
    fn sort_items_scheduled_before_unstreamed_unscheduled() {
        let mut v = vec![new_item(1), new_scheduled_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v2", "v1"]);

        v = vec![new_scheduled_item(1), new_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);
    }

    #[test]
    fn sort_items_streamed() {
        let mut v = vec![new_streamed_item(1), new_streamed_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v2", "v1"]);

        v = vec![new_streamed_item(2), new_streamed_item(1)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v2", "v1"]);
    }

    #[test]
    fn sort_items_streamed_before_scheduled() {
        let mut v = vec![new_scheduled_item(2), new_streamed_item(1)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);

        v = vec![new_streamed_item(1), new_scheduled_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);
    }

    #[test]
    fn sort_items_streamed_before_unstreamed_scheduled() {
        let mut v = vec![new_item(2), new_streamed_item(1)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);

        v = vec![new_streamed_item(1), new_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);
    }

    #[test]
    fn sort_items_unstreamed_unscheduled() {
        let mut v = vec![new_item(1), new_item(2)];
        sort_items(&mut v);
        assert_video_ids(v, vec!["v1", "v2"]);
    }

    fn new_scheduled_item(n: u32) -> Item {
        let mut i = new_item(n);
        i.scheduled_start_time =
            Some(DateTime::parse_from_rfc3339(&format!("2021-09-30T10:55:0{}+01:00", n)).unwrap());
        i
    }

    fn new_streamed_item(n: u32) -> Item {
        let mut i = new_scheduled_item(n);
        i.actual_start_time =
            Some(DateTime::parse_from_rfc3339(&format!("2021-09-30T10:56:0{}+01:00", n)).unwrap());
        i
    }

    fn new_item(n: u32) -> Item {
        assert!(n <= 9);
        Item {
            video_id: format!("v{}", n).to_owned(),
            playlist_item_id: format!("pii{}", n).to_owned(),
            title: format!("video {}", n).to_owned(),
            ..Default::default()
        }
    }

    fn assert_video_ids(v: Vec<Item>, expected: Vec<&str>) {
        assert_eq!(v.len(), expected.len());
        for (n, i) in v.iter().enumerate() {
            assert_eq!(i.video_id, expected.get(n).unwrap().to_owned());
        }
    }
}
