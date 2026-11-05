# The shell entry point is loaded by Android app_process after explicit ADB activation.
-keep class fun.hackerlife.octendra.ShellMain { public static void main(java.lang.String[]); }
-keep class fun.hackerlife.octendra.ShellMain$* { *; }
# LibADB selects the bundled TLS provider by name.
-keep class org.conscrypt.OpenSSLProvider { public <init>(); }
# These pre-Lollipop adapters are unreachable with minSdk 30.
-dontwarn com.android.org.conscrypt.SSLParametersImpl
-dontwarn org.apache.harmony.xnet.provider.jsse.SSLParametersImpl
