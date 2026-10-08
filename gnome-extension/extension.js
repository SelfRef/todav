// Todav panel indicator: shows the pinned list, ticks and adds tasks through the app's D-Bus API.
// Holds no state and does no networking of its own.

import Clutter from 'gi://Clutter';
import Gio from 'gi://Gio';
import GObject from 'gi://GObject';
import St from 'gi://St';

import * as Main from 'resource:///org/gnome/shell/ui/main.js';
import * as PanelMenu from 'resource:///org/gnome/shell/ui/panelMenu.js';
import * as PopupMenu from 'resource:///org/gnome/shell/ui/popupMenu.js';
import {Extension} from 'resource:///org/gnome/shell/extensions/extension.js';

import {BUS_NAME, OBJECT_PATH, TodavProxy} from './dbus.js';

const MAX_TASKS = 25;

const Indicator = GObject.registerClass(
class TodavIndicator extends PanelMenu.Button {
    _init(settings) {
        super._init(0.5, 'Todav');
        this._settings = settings;
        this._proxy = null;

        const box = new St.BoxLayout({style_class: 'panel-status-menu-box'});
        box.add_child(new St.Icon({icon_name: 'checkbox-checked-symbolic', style_class: 'system-status-icon'}));
        this._count = new St.Label({text: '', y_align: Clutter.ActorAlign.CENTER, style_class: 'todav-count'});
        box.add_child(this._count);
        this._warning = new St.Icon({icon_name: 'dialog-warning-symbolic', style_class: 'system-status-icon', visible: false});
        box.add_child(this._warning);
        this.add_child(box);

        // Don't launch the app at Shell start; the first method call (menu open) activates it.
        new TodavProxy(Gio.DBus.session, BUS_NAME, OBJECT_PATH, (proxy, error) => {
            if (error || this._destroyed) {
                if (error)
                    logError(error, 'Todav: cannot create D-Bus proxy');
                return;
            }
            this._proxy = proxy;
            this._signalId = proxy.connectSignal('Changed', () => this._refreshCount());
            this._propsId = proxy.connect('g-properties-changed', () => this._updateState());
            this._ownerId = proxy.connect('notify::g-name-owner', () => {
                this._refreshCount();
                this._updateState();
            });
            this._refreshCount();
            this._updateState();
        }, null, Gio.DBusProxyFlags.DO_NOT_AUTO_START_AT_CONSTRUCTION);

        this._settingsId = settings.connect('changed::pinned-list', () => this._refreshCount());
        // Rebuild on open only: rebuilding while open would drop just-ticked items under the pointer.
        this.menu.connect('open-state-changed', (_menu, open) => {
            if (open)
                this._rebuild().catch(e => this._placeholder(`Todav: ${e.message}`));
        });
        this._placeholder('Starting…');
    }

    _pinned(lists) {
        const href = this._settings.get_string('pinned-list');
        return lists.find(l => l[0] === href) ?? lists[0];
    }

    _updateState() {
        const state = this._proxy?.g_name_owner ? this._proxy.SyncState ?? '' : '';
        this._warning.visible = state.startsWith('error:');
    }

    async _refreshCount() {
        if (!this._proxy?.g_name_owner) {
            this._count.text = '';
            return;
        }
        try {
            const [lists] = await this._proxy.GetListsAsync();
            const pinned = this._pinned(lists);
            this._count.text = pinned && pinned[2] > 0 ? `${pinned[2]}` : '';
        } catch (e) {
            logError(e, 'Todav: GetLists failed');
        }
    }

    _placeholder(text) {
        this.menu.removeAll();
        this.menu.addMenuItem(new PopupMenu.PopupMenuItem(text, {reactive: false}));
    }

    async _rebuild(focusEntry = false) {
        if (!this._proxy)
            return;
        if (!this._proxy.g_name_owner)
            this._placeholder('Starting…');
        const [lists] = await this._proxy.GetListsAsync();
        const pinned = this._pinned(lists);
        this.menu.removeAll();
        if (!pinned) {
            this._placeholder('No task lists — sign in in Todav');
            this._addOpenItem('');
            return;
        }
        const [href, name, open] = pinned;
        this._count.text = open > 0 ? `${open}` : '';

        const switcher = new PopupMenu.PopupSubMenuMenuItem(name);
        for (const [h, n, c] of lists) {
            const item = new PopupMenu.PopupMenuItem(`${n} (${c})`);
            item.setOrnament(h === href ? PopupMenu.Ornament.DOT : PopupMenu.Ornament.NO_DOT);
            item.connect('activate', () => this._settings.set_string('pinned-list', h));
            switcher.menu.addMenuItem(item);
        }
        this.menu.addMenuItem(switcher);

        const [tasks] = await this._proxy.GetTasksAsync(href);
        let category = null;
        for (const [uid, summary, cat, done] of tasks.slice(0, MAX_TASKS)) {
            if (cat !== category) {
                this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem(cat || 'Other'));
                category = cat;
            }
            this.menu.addMenuItem(this._taskItem(uid, summary, done));
        }
        if (tasks.length > MAX_TASKS) {
            this.menu.addMenuItem(new PopupMenu.PopupMenuItem(`+ ${tasks.length - MAX_TASKS} more`, {reactive: false}));
        } else if (tasks.length === 0) {
            this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
            this.menu.addMenuItem(new PopupMenu.PopupMenuItem('All done', {reactive: false}));
        }

        this.menu.addMenuItem(new PopupMenu.PopupSeparatorMenuItem());
        const entryItem = new PopupMenu.PopupBaseMenuItem({reactive: false, can_focus: false});
        const entry = new St.Entry({hint_text: 'Add a task…', can_focus: true, x_expand: true, style_class: 'todav-entry'});
        entry.clutter_text.connect('activate', () => {
            const text = entry.get_text().trim();
            if (!text)
                return;
            entry.set_text('');
            this._proxy.AddTaskAsync(href, text)
                .then(() => this._rebuild(true))
                .catch(e => logError(e, 'Todav: AddTask failed'));
        });
        entryItem.add_child(entry);
        this.menu.addMenuItem(entryItem);
        this._addOpenItem(href);
        if (focusEntry)
            entry.grab_key_focus();
    }

    _taskItem(uid, summary, done) {
        const item = new PopupMenu.PopupMenuItem(summary);
        item.setOrnament(done ? PopupMenu.Ornament.CHECK : PopupMenu.Ornament.NO_DOT);
        // Toggle in place and keep the menu open (no 'activate' emission → menu stays up).
        item.activate = () => {
            done = !done;
            item.setOrnament(done ? PopupMenu.Ornament.CHECK : PopupMenu.Ornament.NO_DOT);
            this._proxy.SetDoneAsync(uid, done).catch(e => logError(e, 'Todav: SetDone failed'));
        };
        return item;
    }

    _addOpenItem(href) {
        const item = new PopupMenu.PopupMenuItem('Open Todav');
        item.connect('activate', () => {
            this._proxy?.ShowListAsync(href).catch(e => logError(e, 'Todav: ShowList failed'));
        });
        this.menu.addMenuItem(item);
    }

    destroy() {
        this._destroyed = true;
        this._settings.disconnect(this._settingsId);
        if (this._proxy) {
            this._proxy.disconnectSignal(this._signalId);
            this._proxy.disconnect(this._propsId);
            this._proxy.disconnect(this._ownerId);
            this._proxy = null;
        }
        super.destroy();
    }
});

export default class TodavExtension extends Extension {
    enable() {
        this._indicator = new Indicator(this.getSettings());
        Main.panel.addToStatusArea(this.uuid, this._indicator);
    }

    disable() {
        this._indicator?.destroy();
        this._indicator = null;
    }
}
