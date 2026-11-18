package fun.hackerlife.octendra;
import android.view.KeyEvent;
import android.view.MotionEvent;
import android.view.Surface;
/** Local Binder only: caller UID is checked by the existing shell helper. */
interface IUiShell {
    String createDisplay(in Surface surface, int width, int height, int dpi);
    String releaseDisplay(int displayId);
    String launchApp(int displayId, String packageName);
    String key(int displayId, int keyCode, int modifiers);
    String motion(int displayId, String kind, float x, float y, float toX, float toY, double scale);
    String listUsers();
    String launchAppAsUser(int displayId, String packageName, int userId);
    int mainTopTaskId();
    String pullSpilledTasks(int displayId, String packageName, int preMainTopTaskId);
    String displayTaskInfo(int displayId);
    void pokeKeepAwake(int displayId);
    boolean injectMotionEvent(int displayId, in MotionEvent event);
    boolean injectKeyEvent(int displayId, in KeyEvent event);
    void cancelAiInput(int displayId);
    void cancelHumanInput(int displayId);
}
