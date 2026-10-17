//! A footage folder opened for editing, with what has been loaded so far.

use std::collections::HashMap;

use crate::activity::{self, Activity};
use crate::audio::{self, Envelope};
use crate::error::{EditError, Result};
use crate::project::{Project, Take};
use crate::transcribe;
use crate::transcript::TakeView;

pub struct Session {
    pub project: Project,
    views: HashMap<String, TakeView>,
    envelopes: HashMap<String, Envelope>,
    activity: HashMap<String, Activity>,
}

impl Session {
    pub fn new(project: Project) -> Self {
        Self { project, views: HashMap::new(), envelopes: HashMap::new(), activity: HashMap::new() }
    }

    pub fn take(&self, id: &str) -> Result<&Take> {
        self.project.take(id)
    }

    /// Loads the transcript view of `take` (it must be transcribed).
    pub fn load_view(&mut self, take: &str) -> Result<&TakeView> {
        let take = self.project.take(take)?.clone();
        if !take.has_sound() {
            return Err(EditError::invalid(format!(
                "take {} ({}) has no sound, so no phrases: use it in a time segment or as an overlay",
                take.id, take.file
            )));
        }
        if !self.views.contains_key(&take.id) {
            let transcript = transcribe::cached(&self.project, &take)?.ok_or_else(|| {
                EditError::invalid(format!(
                    "take {} ({}) is not transcribed yet: call transcribe first",
                    take.id, take.file
                ))
            })?;
            self.views.insert(take.id.clone(), TakeView::new(&take.id, transcript));
        }
        Ok(&self.views[&take.id])
    }

    pub fn view(&self, take: &str) -> Option<&TakeView> {
        self.project.take(take).ok().and_then(|take| self.views.get(&take.id))
    }

    pub fn load_envelope(&mut self, take: &str) -> Result<()> {
        let take = self.project.take(take)?.clone();
        if !self.envelopes.contains_key(&take.id) {
            // Without sound there is nothing to snap to: cuts stay where they are asked.
            let envelope = if take.has_sound() {
                Envelope::of(&audio::read_wav(&audio::speech_audio(&self.project, &take)?)?)
            } else {
                Envelope { db: Vec::new() }
            };
            self.envelopes.insert(take.id.clone(), envelope);
        }
        Ok(())
    }

    pub fn envelope(&self, take: &str) -> Option<&Envelope> {
        self.project.take(take).ok().and_then(|take| self.envelopes.get(&take.id))
    }

    pub fn load_activity(&mut self, take: &str) -> Result<&Activity> {
        let take = self.project.take(take)?.clone();
        if !self.activity.contains_key(&take.id) {
            let activity = activity::activity(&self.project, &take)?;
            self.activity.insert(take.id.clone(), activity);
        }
        Ok(&self.activity[&take.id])
    }

    pub fn activity(&self, take: &str) -> Option<&Activity> {
        self.project.take(take).ok().and_then(|take| self.activity.get(&take.id))
    }

    /// Forgets loaded transcripts (after a new transcription).
    pub fn reset_views(&mut self) {
        self.views.clear();
    }
}
