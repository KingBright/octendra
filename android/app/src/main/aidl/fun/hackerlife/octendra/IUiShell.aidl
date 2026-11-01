package fun.hackerlife.octendra;
import android.view.Surface;
/** Local Binder only: caller UID is checked by the existing shell helper. */
interface IUiShell {
    String createDisplay(in Surface surface, int width, int height, int dpi);
    String releaseDisplay(int displayId);
    String launchApp(int displayId, String packageName);
    String key(int displayId, int keyCode, int modifiers);
}
