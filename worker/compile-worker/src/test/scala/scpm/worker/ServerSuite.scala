package scpm.worker

import java.io.{ ByteArrayInputStream, ByteArrayOutputStream }
import java.nio.charset.StandardCharsets.UTF_8
import java.nio.file.Files
import java.util.concurrent.atomic.AtomicBoolean

class ServerSuite extends munit.FunSuite:
  /** Compiles by target name: `ok`, `boom` (throws) or `oom` (a fatal JVM error). */
  private object FakeCompiler extends CompileService:
    def compile(id: Long, params: CompileParams, cancelled: AtomicBoolean, emit: Event => Unit): CompileResult =
      params.target match
        case "boom" => throw IllegalStateException("boom")
        case "oom"  => throw OutOfMemoryError("fake")
        case "soe"  => throw StackOverflowError()
        case _      => CompileResult(CompileStatus.Ok, 1, 1, noop = false)

  private def serve(lines: String*): (List[String], List[Int]) =
    val out = ByteArrayOutputStream()
    val exits = List.newBuilder[Int]
    Server(Output(out), FakeCompiler, exits += _)
      .serve(ByteArrayInputStream(lines.mkString("", "\n", "\n").getBytes(UTF_8)))
    (out.toString(UTF_8).linesIterator.toList, exits.result())

  private val jar = Files.createTempFile("scpm", ".jar")
  private def compile(id: Int, target: String) =
    s"""{"id":$id,"method":"compile","params":{"target":"$target","scalaVersion":"3.9.0",
       |"scalaCompilerClasspath":["$jar"],"scalaLibraryClasspath":[],"compilerBridge":"$jar",
       |"sources":[],"classesDir":"/tmp/scpm-test/classes","analysisFile":"/tmp/scpm-test/a.zip"}}""".stripMargin
      .replace("\n", "")

  test("answers initialize, rejects bad requests and stops at shutdown") {
    val (responses, _) = serve(
      """{"id":1,"method":"initialize","params":{"protocolVersion":1}}""",
      """not json""",
      """{"id":3,"method":"nope"}""",
      """{"id":4,"method":"cancel","params":{"requestId":99}}""",
      """{"id":5,"method":"shutdown"}""",
      """{"id":6,"method":"initialize","params":{"protocolVersion":1}}"""
    )
    assertEquals(responses.length, 5)
    assert(responses(0).startsWith("""{"id":1,"result":{"protocolVersion":1,"workerVersion":"""), responses(0))
    assert(responses(1).startsWith("""{"id":null,"error":{"code":"bad_request""""), responses(1))
    assert(responses(2).startsWith("""{"id":3,"error":{"code":"unknown_method""""), responses(2))
    assertEquals(responses(3), """{"id":4,"result":{}}""")
    assertEquals(responses(4), """{"id":5,"result":{}}""")
  }

  test("a request missing fields is answered with its own id") {
    val (responses, _) = serve("""{"id":7,"params":{}}""", """{"id":8,"method":"compile","params":{"target":"x"}}""")
    assert(responses(0).startsWith("""{"id":7,"error":{"code":"bad_request""""), responses(0))
    assert(responses(1).startsWith("""{"id":8,"error":{"code":"bad_request""""), responses(1))
  }

  test("invalid paths are refused before compiling") {
    val (responses, _) = serve(compile(1, "ok").replace(jar.toString, "/no/such.jar"))
    assert(responses(0).contains("no such file: /no/such.jar"), responses(0))
  }

  test("a compile that throws is answered with an internal error") {
    val (responses, exits) = serve(compile(1, "boom"))
    assertEquals(
      responses,
      List("""{"id":1,"error":{"code":"internal","message":"java.lang.IllegalStateException: boom"}}""")
    )
    assertEquals(exits, Nil)
  }

  test("a stack overflow is answered with crashed and the worker keeps serving") {
    val (responses, exits) = serve(compile(1, "soe"), compile(2, "ok"))
    assert(responses.exists(_.startsWith("""{"id":1,"result":{"status":"crashed"""")), responses)
    assert(responses.exists(_.startsWith("""{"id":2,"result":{"status":"ok"""")), responses)
    assertEquals(exits, Nil)
  }

  test("a fatal JVM error is answered with crashed, then the worker exits 3") {
    val (responses, exits) = serve(compile(1, "oom"))
    assert(responses(0).startsWith("""{"id":1,"result":{"status":"crashed""""), responses(0))
    assertEquals(exits, List(3))
  }
