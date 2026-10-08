import Gio from 'gi://Gio';

export const BUS_NAME = 'io.github.selfref.Todav';
export const OBJECT_PATH = '/io/github/selfref/Todav';

// Keep in sync with linux/app/src/dbus.rs.
const XML = `<node>
<interface name="io.github.selfref.Todav1">
  <method name="GetLists"><arg direction="out" type="a(ssu)"/></method>
  <method name="GetTasks"><arg direction="in" name="list" type="s"/><arg direction="out" type="a(sssb)"/></method>
  <method name="SetDone"><arg direction="in" name="uid" type="s"/><arg direction="in" name="done" type="b"/></method>
  <method name="AddTask"><arg direction="in" name="list" type="s"/><arg direction="in" name="summary" type="s"/><arg direction="out" type="s"/></method>
  <method name="ShowList"><arg direction="in" name="list" type="s"/></method>
  <method name="Sync"/>
  <signal name="Changed"><arg name="list" type="s"/></signal>
  <property name="SyncState" type="s" access="read"/>
</interface>
</node>`;

export const TodavProxy = Gio.DBusProxy.makeProxyWrapper(XML);
