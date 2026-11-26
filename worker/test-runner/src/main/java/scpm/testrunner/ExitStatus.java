package scpm.testrunner;

import java.util.OptionalInt;
import java.util.logging.Handler;
import java.util.logging.Level;
import java.util.logging.LogRecord;
import java.util.logging.Logger;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/**
 * Learns the status a test passed to {@code System.exit}. Since JDK 21 the JDK logs every
 * {@code Runtime.exit} to the {@code java.lang.Runtime} logger, before shutdown hooks run; on
 * older JDKs the status stays unknown.
 */
final class ExitStatus {
  private static final Pattern STATUS = Pattern.compile("Runtime\\.exit\\((-?\\d+)\\)");
  // Held strongly: java.util.logging only keeps weak references to loggers.
  private static final Logger RUNTIME = Logger.getLogger("java.lang.Runtime");
  private static volatile Integer status;

  private ExitStatus() {}

  static void capture() {
    RUNTIME.setLevel(Level.FINE);
    RUNTIME.setUseParentHandlers(false);
    RUNTIME.addHandler(
        new Handler() {
          @Override
          public void publish(LogRecord record) {
            if (record.getThrown() == null) return;
            Matcher m = STATUS.matcher(String.valueOf(record.getThrown().getMessage()));
            if (m.find()) status = Integer.valueOf(m.group(1));
          }

          @Override
          public void flush() {}

          @Override
          public void close() {}
        });
  }

  static OptionalInt last() {
    Integer s = status;
    return s == null ? OptionalInt.empty() : OptionalInt.of(s);
  }
}
