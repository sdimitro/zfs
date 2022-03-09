use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;

use derivative::Derivative;
use futures::stream;
use log::info;
use serde::Deserialize;
use serde::Serialize;
use tokio::sync::mpsc;
use zettacache::base_types::PoolGuid;

use crate::base_types::ObjectId;
use crate::data_object::DataObject;
use crate::object_access::OBJECT_DELETION_BATCH_SIZE;
use crate::ObjectAccess;

pub struct ObjectDeleter {
    // objects to delete at the end of this txg
    objects_to_delete: Option<Vec<ObjectId>>,
    // objects that are being deleted by background task
    obsolete_objects: VecDeque<ObjectId>,
    rx: mpsc::UnboundedReceiver<usize>,
    tx: mpsc::UnboundedSender<Vec<ObjectId>>,
}

#[derive(Default, Derivative, Serialize, Deserialize, Clone)]
#[derivative(Debug)]
pub struct ObjectDeleterPhys(#[derivative(Debug(format_with = "util::tersevec"))] Vec<ObjectId>);

impl ObjectDeleter {
    pub fn new(object_access: Arc<ObjectAccess>, guid: PoolGuid) -> ObjectDeleter {
        Self::open(object_access, guid, Default::default())
    }

    async fn delete_task(
        object_access: Arc<ObjectAccess>,
        guid: PoolGuid,
        mut rx: mpsc::UnboundedReceiver<Vec<ObjectId>>,
        tx: mpsc::UnboundedSender<usize>,
    ) {
        while let Some(objects) = rx.recv().await {
            let begin = Instant::now();
            let len = objects.len();
            // Do the chunking here so that we can transmit progress after each DeleteObjects call.
            for chunk in objects.chunks(*OBJECT_DELETION_BATCH_SIZE) {
                let vec = chunk.to_owned();
                object_access
                    .delete_objects(stream::iter(
                        vec.into_iter().map(|o| DataObject::key(guid, o)),
                    ))
                    .await;
                if tx.send(chunk.len()).is_err() {
                    // ObjectDeleter has been dropped
                    return;
                }
            }
            info!(
                "reclaim: deleted {} objects in {}ms",
                len,
                begin.elapsed().as_millis()
            );
        }
    }

    pub fn open(
        object_access: Arc<ObjectAccess>,
        guid: PoolGuid,
        phys: ObjectDeleterPhys,
    ) -> ObjectDeleter {
        let (completion_tx, completion_rx) = mpsc::unbounded_channel();
        let (initiation_tx, initiation_rx) = mpsc::unbounded_channel();

        let mut object_deleter = ObjectDeleter {
            objects_to_delete: Default::default(),
            obsolete_objects: Default::default(),
            rx: completion_rx,
            tx: initiation_tx,
        };

        if !object_access.readonly() {
            tokio::spawn(Self::delete_task(
                object_access,
                guid,
                initiation_rx,
                completion_tx,
            ));
            // resume deleting what's on the queue, leaving it in obsolete_objects
            object_deleter.delete(phys.0);
            object_deleter.sync_done();
        }

        object_deleter
    }

    /// Schedule objects for deletion after this txg ends.  We won't start
    /// deleting them until `sync_done()` is called.  Note that this can only be
    /// called once per txg (i.e. only one call to delete() before the next call
    /// to sync_done()).
    pub fn delete(&mut self, objects: Vec<ObjectId>) {
        assert!(self.objects_to_delete.is_none());
        info!("reclaim: logged {} deleted objects", objects.len());
        self.objects_to_delete = Some(objects);
    }

    pub fn phys(&mut self) -> ObjectDeleterPhys {
        while let Ok(completed) = self.rx.try_recv() {
            self.obsolete_objects.drain(..completed);
        }
        ObjectDeleterPhys(
            self.obsolete_objects
                .iter()
                .chain(self.objects_to_delete.as_ref().unwrap_or(&Vec::new()))
                .copied()
                .collect(),
        )
    }

    /// Notify that the txg has been synced, so we can now start deleting the objects that were
    /// pushed. Panics if called with a readonly ObjectAccess.
    pub fn sync_done(&mut self) {
        if let Some(objects_to_delete) = self.objects_to_delete.take() {
            self.obsolete_objects.extend(&objects_to_delete);
            self.tx.send(objects_to_delete).unwrap();
        }
    }
}
