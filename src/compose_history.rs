//! Compose history includes paintables and formatting, which GtkTextBuffer's
//! built-in text-only undo recorder does not preserve.
use std::{
    cell::{Cell, RefCell},
    collections::VecDeque,
    rc::Rc,
};

use gtk::{gdk, gio, glib, prelude::*};

mod imp {
    use gtk::{glib, prelude::*, subclass::prelude::*};

    #[derive(Default)]
    pub struct Editor;

    #[glib::object_subclass]
    impl ObjectSubclass for Editor {
        const NAME: &'static str = "PostbirdComposeEditor";
        type Type = super::Editor;
        type ParentType = gtk::TextView;

        fn class_init(class: &mut Self::Class) {
            for action in ["text.undo", "text.redo"] {
                class.install_action(action, None, |editor, action, _| {
                    let action = action.replace("text.", "compose.");
                    let _ = editor.activate_action(&action, None);
                });
            }
        }
    }

    impl ObjectImpl for Editor {}
    impl WidgetImpl for Editor {}
    impl TextViewImpl for Editor {}
}

glib::wrapper! {
    pub struct Editor(ObjectSubclass<imp::Editor>)
        @extends gtk::TextView, gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget, gtk::Scrollable;
}

pub fn editor() -> gtk::TextView {
    glib::Object::new::<Editor>().upcast()
}

#[derive(Clone, PartialEq, Eq)]
struct Run {
    text: String,
    image: Option<gdk::Paintable>,
    tags: Vec<gtk::TextTag>,
}

#[derive(Clone)]
struct Snapshot {
    runs: Vec<Run>,
    cursor: i32,
    selection: i32,
}

impl Snapshot {
    fn capture(buffer: &gtk::TextBuffer) -> Self {
        let mut runs: Vec<Run> = Vec::new();
        let mut iter = buffer.start_iter();
        while !iter.is_end() {
            let image = iter.paintable();
            let tags = iter.tags();
            if image.is_none()
                && let Some(last) = runs.last_mut()
                && last.image.is_none()
                && last.tags == tags
            {
                last.text.push(iter.char());
            } else {
                runs.push(Run {
                    text: if image.is_some() {
                        String::new()
                    } else {
                        iter.char().to_string()
                    },
                    image,
                    tags,
                });
            }
            iter.forward_char();
        }
        Self {
            runs,
            cursor: buffer.cursor_position(),
            selection: buffer.iter_at_mark(&buffer.selection_bound()).offset(),
        }
    }

    fn restore(&self, buffer: &gtk::TextBuffer) {
        buffer.set_text("");
        for run in &self.runs {
            let start = buffer.end_iter().offset();
            if let Some(image) = &run.image {
                buffer.insert_paintable(&mut buffer.end_iter(), image);
            } else {
                buffer.insert(&mut buffer.end_iter(), &run.text);
            }
            for tag in &run.tags {
                buffer.apply_tag(tag, &buffer.iter_at_offset(start), &buffer.end_iter());
            }
        }
        buffer.select_range(
            &buffer.iter_at_offset(self.cursor),
            &buffer.iter_at_offset(self.selection),
        );
    }
}

struct History {
    current: Snapshot,
    undo: VecDeque<Snapshot>,
    redo: Vec<Snapshot>,
}

/// Replace the editor's standard undo/redo actions, so both its keyboard
/// shortcuts and context menu use the same image-aware history.
pub fn install(editor: &gtk::TextView) {
    let buffer = editor.buffer();
    buffer.set_enable_undo(false);
    let history = Rc::new(RefCell::new(History {
        current: Snapshot::capture(&buffer),
        undo: VecDeque::new(),
        redo: Vec::new(),
    }));
    let restoring = Rc::new(Cell::new(false));
    let depth = Rc::new(Cell::new(0usize));
    let undo = gio::SimpleAction::new("undo", None);
    let redo = gio::SimpleAction::new("redo", None);
    undo.set_enabled(false);
    redo.set_enabled(false);
    let group = gio::SimpleActionGroup::new();
    group.add_action(&undo);
    group.add_action(&redo);
    editor.insert_action_group("compose", Some(&group));

    let record: Rc<dyn Fn(&gtk::TextBuffer)> = {
        let history = history.clone();
        let restoring = restoring.clone();
        let depth = depth.clone();
        let undo = undo.clone();
        let redo = redo.clone();
        let editor = editor.downgrade();
        Rc::new(move |buffer| {
            if restoring.get() || depth.get() > 0 {
                return;
            }
            let next = Snapshot::capture(buffer);
            let mut history = history.borrow_mut();
            if next.runs != history.current.runs {
                let previous = std::mem::replace(&mut history.current, next);
                history.undo.push_back(previous);
                // Bound history memory; image snapshots share the original texture.
                if history.undo.len() > 100 {
                    history.undo.pop_front();
                }
                history.redo.clear();
            }
            undo.set_enabled(!history.undo.is_empty());
            redo.set_enabled(!history.redo.is_empty());
            if let Some(editor) = editor.upgrade() {
                editor.action_set_enabled("text.undo", undo.is_enabled());
                editor.action_set_enabled("text.redo", redo.is_enabled());
            }
        })
    };
    {
        let record = record.clone();
        buffer.connect_changed(move |buffer| record(buffer));
    }
    for signal in ["apply-tag", "remove-tag"] {
        let record = record.clone();
        buffer.connect_local(signal, true, move |values| {
            record(&values[0].get::<gtk::TextBuffer>().unwrap());
            None
        });
    }
    {
        let depth = depth.clone();
        let history = history.clone();
        let restoring = restoring.clone();
        buffer.connect_begin_user_action(move |buffer| {
            if depth.get() == 0 && !restoring.get() {
                let mut history = history.borrow_mut();
                history.current.cursor = buffer.cursor_position();
                history.current.selection = buffer.iter_at_mark(&buffer.selection_bound()).offset();
            }
            depth.set(depth.get() + 1);
        });
    }
    buffer.connect_end_user_action(move |buffer| {
        depth.set(depth.get().saturating_sub(1));
        record(buffer);
    });

    for (action, is_redo) in [(&undo, false), (&redo, true)] {
        let history = history.clone();
        let restoring = restoring.clone();
        let editor = editor.downgrade();
        let undo = undo.downgrade();
        let redo = redo.downgrade();
        action.connect_activate(move |_, _| {
            let Some(editor) = editor
                .upgrade()
                .filter(|editor| editor.is_editable() && editor.is_sensitive())
            else {
                return;
            };
            let buffer = editor.buffer();
            let target = {
                let mut history = history.borrow_mut();
                let target = if is_redo {
                    history.redo.pop()
                } else {
                    history.undo.pop_back()
                };
                let Some(target) = target else {
                    return;
                };
                let previous = Snapshot::capture(&buffer);
                if is_redo {
                    history.undo.push_back(previous);
                } else {
                    history.redo.push(previous);
                }
                history.current = target.clone();
                target
            };
            restoring.set(true);
            target.restore(&buffer);
            restoring.set(false);
            editor.scroll_mark_onscreen(&buffer.get_insert());
            if let (Some(undo), Some(redo)) = (undo.upgrade(), redo.upgrade()) {
                let history = history.borrow();
                undo.set_enabled(!history.undo.is_empty());
                redo.set_enabled(!history.redo.is_empty());
                editor.action_set_enabled("text.undo", undo.is_enabled());
                editor.action_set_enabled("text.redo", redo.is_enabled());
            }
        });
    }
}
