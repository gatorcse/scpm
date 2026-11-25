package scpm.worker

import java.io.{ ByteArrayInputStream, ByteArrayOutputStream }
import java.nio.charset.StandardCharsets.UTF_8

class ServerSuite extends munit.FunSuite:
  private def serve(lines: String*): List[String] =
    val out = ByteArrayOutputStream()
    Server(Output(out)).serve(ByteArrayInputStream(lines.mkString("", "\n", "\n").getBytes(UTF_8)))
    out.toString(UTF_8).linesIterator.toList

  test("answers initialize, rejects bad requests and stops at shutdown") {
    val responses = serve(
      """{"id":1,"method":"initialize","params":{"protocolVersion":1}}""",
      """not json""",
      """{"id":2,"method":"compile","params":{"target":"x"}}""",
      """{"id":3,"method":"nope"}""",
      """{"id":4,"method":"cancel","params":{"requestId":99}}""",
      """{"id":5,"method":"shutdown"}""",
      """{"id":6,"method":"initialize","params":{"protocolVersion":1}}"""
    )
    assertEquals(responses.length, 6)
    assert(responses(0).startsWith("""{"id":1,"result":{"protocolVersion":1,"workerVersion":"""), responses(0))
    assert(responses(1).startsWith("""{"id":null,"error":{"code":"bad_request""""), responses(1))
    assert(responses(2).startsWith("""{"id":2,"error":{"code":"bad_request""""), responses(2))
    assert(responses(3).startsWith("""{"id":3,"error":{"code":"unknown_method""""), responses(3))
    assertEquals(responses(4), """{"id":4,"result":{}}""")
    assertEquals(responses(5), """{"id":5,"result":{}}""")
  }
