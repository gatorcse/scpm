package scpm.testrunner;

import java.util.List;
import java.util.Map;
import java.util.regex.Pattern;

/** Parameters of the {@code run} request. Optional lists are empty when absent. */
record RunParams(
    String target,
    List<String> testClassesDirs,
    List<String> frameworks,
    List<Pattern> filters,
    List<String> testNames) {

  static RunParams fromJson(Map<String, Object> params) {
    return new RunParams(
        string(params, "target"),
        strings(params, "testClassesDirs", true),
        strings(params, "frameworks", false),
        strings(params, "filters", false).stream().map(RunParams::glob).toList(),
        strings(params, "testNames", false));
  }

  boolean selects(String className) {
    return filters.isEmpty() || filters.stream().anyMatch(p -> p.matcher(className).matches());
  }

  /** {@code *} matches any run of characters, {@code ?} any single character. */
  static Pattern glob(String glob) {
    StringBuilder regex = new StringBuilder();
    for (String literal : glob.split("((?<=[*?])|(?=[*?]))")) {
      switch (literal) {
        case "*" -> regex.append(".*");
        case "?" -> regex.append('.');
        default -> regex.append(Pattern.quote(literal));
      }
    }
    return Pattern.compile(regex.toString());
  }

  private static String string(Map<String, Object> params, String key) {
    if (params.get(key) instanceof String s) return s;
    throw new IllegalArgumentException("'" + key + "' must be a string");
  }

  private static List<String> strings(Map<String, Object> params, String key, boolean required) {
    Object value = params.get(key);
    if (value == null && !required) return List.of();
    if (value instanceof List<?> list && list.stream().allMatch(x -> x instanceof String)) {
      return list.stream().map(String.class::cast).toList();
    }
    throw new IllegalArgumentException("'" + key + "' must be an array of strings");
  }
}
