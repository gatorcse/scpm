package arith

class HangingHookSuite extends munit.FunSuite:
  override val munitTimeout = scala.concurrent.duration.Duration(5, "min")

  test("registers a shutdown hook that never finishes") {
    Runtime.getRuntime.addShutdownHook(Thread(() => Thread.sleep(Long.MaxValue)))
    println("hook registered")
    Thread.sleep(60000)
  }
