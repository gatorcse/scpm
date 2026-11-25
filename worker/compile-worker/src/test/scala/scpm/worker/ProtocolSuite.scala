package scpm.worker

import com.github.plokhotnyuk.jsoniter_scala.core.*

class ProtocolSuite extends munit.FunSuite:
  test("requests ignore unknown fields and default missing params") {
    val request = readFromString[Request]("""{"id":7,"method":"initialize","extra":[1,2]}""")
    assertEquals((request.id, request.method), (7L, "initialize"))
    assertEquals(writeToString(request.params), "{}")
  }

  test("compile params fill in optional fields") {
    val json =
      """{"target":"core@jvm-3","scalaVersion":"3.9.0","scalaCompilerClasspath":["c.jar"],
        |"scalaLibraryClasspath":["l.jar"],"compilerBridge":"b.jar","sources":["/a.scala"],
        |"classesDir":"/out","analysisFile":"/out.zip","future":true}""".stripMargin
    val params = readFromString[Request](s"""{"id":1,"method":"compile","params":$json}""").params.decode[CompileParams]
    assertEquals(params.classpath, Nil)
    assertEquals(params.javaHome, None)
    assertEquals(params.rootPaths, Map.empty)
  }

  test("compile params require sources, but accept an empty list") {
    val base =
      """"target":"t","scalaVersion":"3.9.0","scalaCompilerClasspath":["c.jar"],
        |"scalaLibraryClasspath":["l.jar"],"compilerBridge":"b.jar","classesDir":"/out","analysisFile":"/out.zip"""".stripMargin
    intercept[JsonReaderException](readFromString[CompileParams](s"{$base}"))
    val empty = readFromString[CompileParams](s"""{$base,"sources":[]}""")
    assertEquals(empty.sources, Nil)
  }

  test("compile results write the status as a string and omit an absent message") {
    val json = writeToString(CompileResult(CompileStatus.Cancelled, 12, 0, noop = false))
    assertEquals(json, """{"status":"cancelled","durationMs":12,"compiledSources":0,"noop":false}""")
  }

  test("events carry their type in `t` and spell out unknown values as null") {
    val event: Event = Event.Diagnostic(1, "core", "error", None, None, None, "boom", None, Nil)
    assertEquals(
      writeToString(Notification(event)),
      """{"params":{"t":"diagnostic","requestId":1,"target":"core","severity":"error","file":null,""" +
        """"range":null,"code":null,"message":"boom","rendered":null,"actions":[]},"method":"event"}"""
    )
  }
