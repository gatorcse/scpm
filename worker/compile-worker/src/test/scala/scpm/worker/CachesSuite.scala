package scpm.worker

import java.nio.file.attribute.FileTime
import java.nio.file.{ Files, Path, StandardCopyOption }
import scala.collection.mutable

class CachesSuite extends munit.FunSuite:
  test("LeasingCache keeps the most recently used entries and closes the rest") {
    val closed = mutable.Buffer.empty[String]
    val cache = LeasingCache[String, String](2, k => s"v$k", closed += _)
    cache.use("a")(identity)
    cache.use("b")(identity)
    cache.use("a")(identity)
    cache.use("c")(identity)
    assertEquals(closed.toList, List("vb"))
    assertEquals(cache.size, 2)
  }

  test("LeasingCache closes an evicted value only after its last lease") {
    val closed = mutable.Buffer.empty[String]
    val cache = LeasingCache[String, String](8, k => s"v$k", closed += _)
    cache.use("a") { _ =>
      cache.evict("a")
      assertEquals(closed.toList, Nil)
    }
    assertEquals(closed.toList, List("va"))
  }

  private val dir = FunFixture[Path](_ => Files.createTempDirectory("scpm-cache"), d => sbt.io.IO.delete(d.toFile))

  dir.test("FileCache reloads a file whose size or identity changed, even with the same mtime") { dir =>
    val loads = mutable.Buffer.empty[String]
    val cache = FileCache[String](8, p => Some(Files.readString(p)).tap(loads ++= _))
    val file = dir.resolve("analysis")
    val mtime = FileTime.fromMillis(1_000_000)
    def write(content: String, target: Path = file) =
      Files.writeString(target, content)
      Files.setLastModifiedTime(target, mtime)

    write("one")
    assertEquals(cache.read(file), Some("one"))
    assertEquals(cache.read(file), Some("one"))
    write("three")
    assertEquals(cache.read(file), Some("three"))
    val replacement = dir.resolve("replacement")
    write("other", replacement)
    Files.move(replacement, file, StandardCopyOption.REPLACE_EXISTING, StandardCopyOption.ATOMIC_MOVE)
    assertEquals(cache.read(file), Some("other"))
    assertEquals(loads.toList, List("one", "three", "other"))
    Files.delete(file)
    assertEquals(cache.read(file), None)
  }

  dir.test("FileCache holds at most maxEntries files") { dir =>
    var loads = 0
    val cache = FileCache[String](1, p => { loads += 1; Some(Files.readString(p)) })
    val a = Files.writeString(dir.resolve("a"), "a")
    val b = Files.writeString(dir.resolve("b"), "b")
    cache.read(a)
    cache.read(b)
    cache.read(a)
    assertEquals(loads, 3)
  }

  extension [A](a: A) private def tap(f: A => Unit): A = { f(a); a }
