package arith

class SleepSuite extends munit.FunSuite:
  override val munitTimeout = scala.concurrent.duration.Duration(5, "min")

  test("sleeps, with a child process") {
    val child = ProcessBuilder("sleep", "300").start()
    println(s"child pid ${child.pid}")
    Thread.sleep(60000)
  }
