package arith

class SleepSuite extends munit.FunSuite:
  override val munitTimeout = scala.concurrent.duration.Duration(5, "min")

  test("sleeps") {
    Thread.sleep(60000)
  }
