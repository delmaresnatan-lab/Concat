// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah and Concat contributors
//! Compound clips: grouping a selection into one clip that reads a
//! sub-timeline, and taking it apart again. No render anywhere - the
//! heavy lifting is the flatten the preview and export already run -
//! so both verbs are plain commands, not jobs.

use super::*;
use concat_project::commands::compound_selection;

impl Studio {
    /// What stops the selection becoming a compound clip, or `None` when
    /// the row lights and the chord goes through.
    pub fn compound_blocked(&self) -> Option<String> {
        if self.selection.iter().any(|id| {
            self.clip(id)
                .is_some_and(|clip| self.locked(&clip.track_id))
        }) {
            return Some(t("studio.mergeTrackLocked"));
        }
        compound_selection(self.timeline(), &self.selection)
            .err()
            .map(|error| error.to_string())
    }

    /// The clip "undo compound clip" acts on: the one a menu targets, or
    /// the sole selected one, when either is a compound clip.
    pub(super) fn compound_target(&self) -> Option<String> {
        let target = self
            .menu_target
            .clone()
            .or_else(|| self.sole_selection());
        target.filter(|id| {
            self.clip(id)
                .is_some_and(|clip| clip.kind == model::ClipKind::Compound)
        })
    }

    /// Group the selection into one compound clip, which is selected in
    /// place of what it gathered.
    pub fn create_compound(&mut self) {
        if let Some(reason) = self.compound_blocked() {
            self.notify(&reason, false);
            return;
        }
        let ids = self.selection.clone();
        let created = self.apply(Command::CompoundClips { clip_ids: ids });
        if let Some(id) = created {
            self.selection = vec![id];
            self.transition_selected = None;
        }
    }

    /// Take a compound clip apart, restoring what it gathered.
    pub fn undo_compound(&mut self) {
        let Some(id) = self.compound_target() else {
            return;
        };
        self.apply(Command::UncompoundClips { clip_id: id });
    }
}
