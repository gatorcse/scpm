package scpm.testrunner;

import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;

/**
 * Just enough JSON for one request in and a stream of events out. The runner shares its classpath
 * with the code under test, so it cannot depend on a JSON library.
 *
 * <p>Values map to {@code Map<String, Object>}, {@code List<Object>}, {@code String}, {@code Long}
 * or {@code Double}, {@code Boolean} and {@code null}.
 */
final class Json {
  private Json() {}

  static Object parse(String text) {
    Parser p = new Parser(text);
    Object value = p.value();
    p.skipWhitespace();
    if (p.pos != text.length()) throw p.error("trailing characters");
    return value;
  }

  static String write(Object value) {
    StringBuilder sb = new StringBuilder();
    write(value, sb);
    return sb.toString();
  }

  private static void write(Object value, StringBuilder sb) {
    if (value == null) {
      sb.append("null");
    } else if (value instanceof String s) {
      writeString(s, sb);
    } else if (value instanceof Number || value instanceof Boolean) {
      sb.append(value);
    } else if (value instanceof Map<?, ?> map) {
      sb.append('{');
      boolean first = true;
      for (Map.Entry<?, ?> e : map.entrySet()) {
        if (!first) sb.append(',');
        first = false;
        writeString(e.getKey().toString(), sb);
        sb.append(':');
        write(e.getValue(), sb);
      }
      sb.append('}');
    } else if (value instanceof Iterable<?> items) {
      sb.append('[');
      boolean first = true;
      for (Object item : items) {
        if (!first) sb.append(',');
        first = false;
        write(item, sb);
      }
      sb.append(']');
    } else {
      throw new IllegalArgumentException("not a JSON value: " + value.getClass());
    }
  }

  private static void writeString(String s, StringBuilder sb) {
    sb.append('"');
    for (int i = 0; i < s.length(); i++) {
      char c = s.charAt(i);
      switch (c) {
        case '"' -> sb.append("\\\"");
        case '\\' -> sb.append("\\\\");
        case '\n' -> sb.append("\\n");
        case '\r' -> sb.append("\\r");
        case '\t' -> sb.append("\\t");
        default -> {
          if (c < 0x20) sb.append(String.format("\\u%04x", (int) c));
          else sb.append(c);
        }
      }
    }
    sb.append('"');
  }

  private static final class Parser {
    private final String s;
    private int pos;

    Parser(String s) {
      this.s = s;
    }

    Object value() {
      skipWhitespace();
      if (pos >= s.length()) throw error("unexpected end of input");
      char c = s.charAt(pos);
      return switch (c) {
        case '{' -> object();
        case '[' -> array();
        case '"' -> string();
        case 't' -> literal("true", Boolean.TRUE);
        case 'f' -> literal("false", Boolean.FALSE);
        case 'n' -> literal("null", null);
        default -> number();
      };
    }

    private Map<String, Object> object() {
      Map<String, Object> map = new LinkedHashMap<>();
      pos++;
      skipWhitespace();
      if (peek() == '}') {
        pos++;
        return map;
      }
      while (true) {
        skipWhitespace();
        if (peek() != '"') throw error("expected a string key");
        String key = string();
        skipWhitespace();
        expect(':');
        map.put(key, value());
        skipWhitespace();
        if (peek() == ',') {
          pos++;
        } else {
          expect('}');
          return map;
        }
      }
    }

    private List<Object> array() {
      List<Object> list = new ArrayList<>();
      pos++;
      skipWhitespace();
      if (peek() == ']') {
        pos++;
        return list;
      }
      while (true) {
        list.add(value());
        skipWhitespace();
        if (peek() == ',') {
          pos++;
        } else {
          expect(']');
          return list;
        }
      }
    }

    private String string() {
      pos++;
      StringBuilder sb = new StringBuilder();
      while (true) {
        if (pos >= s.length()) throw error("unterminated string");
        char c = s.charAt(pos++);
        if (c == '"') return sb.toString();
        if (c != '\\') {
          sb.append(c);
          continue;
        }
        if (pos >= s.length()) throw error("unterminated escape");
        char e = s.charAt(pos++);
        switch (e) {
          case '"', '\\', '/' -> sb.append(e);
          case 'b' -> sb.append('\b');
          case 'f' -> sb.append('\f');
          case 'n' -> sb.append('\n');
          case 'r' -> sb.append('\r');
          case 't' -> sb.append('\t');
          case 'u' -> {
            if (pos + 4 > s.length()) throw error("bad unicode escape");
            sb.append((char) Integer.parseInt(s.substring(pos, pos + 4), 16));
            pos += 4;
          }
          default -> throw error("bad escape \\" + e);
        }
      }
    }

    private Object number() {
      int start = pos;
      while (pos < s.length() && "+-0123456789.eE".indexOf(s.charAt(pos)) >= 0) pos++;
      String n = s.substring(start, pos);
      if (n.isEmpty()) throw error("unexpected character");
      try {
        return n.contains(".") || n.contains("e") || n.contains("E")
            ? (Object) Double.parseDouble(n)
            : (Object) Long.parseLong(n);
      } catch (NumberFormatException e) {
        throw error("bad number " + n);
      }
    }

    private Object literal(String word, Object value) {
      if (!s.startsWith(word, pos)) throw error("unexpected token");
      pos += word.length();
      return value;
    }

    private char peek() {
      if (pos >= s.length()) throw error("unexpected end of input");
      return s.charAt(pos);
    }

    private void expect(char c) {
      if (peek() != c) throw error("expected '" + c + "'");
      pos++;
    }

    void skipWhitespace() {
      while (pos < s.length() && Character.isWhitespace(s.charAt(pos))) pos++;
    }

    IllegalArgumentException error(String message) {
      return new IllegalArgumentException("invalid JSON at offset " + pos + ": " + message);
    }
  }
}
