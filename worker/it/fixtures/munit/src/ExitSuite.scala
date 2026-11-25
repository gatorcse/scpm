package arith

class ExitSuite extends munit.FunSuite:
  test("exits the JVM") {
    sys.exit(3)
  }
