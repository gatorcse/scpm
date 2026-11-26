package shapes;

public final class Units {
  private Units() {}

  public static String format(double value) {
    return String.format("%.2f units", value);
  }
}
