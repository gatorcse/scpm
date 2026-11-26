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

  private val unwritable = new xsbti.compile.AnalysisContents:
    def getAnalysis = throw IllegalStateException("cannot serialize")
    def getMiniSetup = throw IllegalStateException("cannot serialize")

  private def emptyAnalysis(dir: Path) =
    import xsbti.compile.{ AnalysisContents, CompileOrder, MiniOptions, MiniSetup }
    val options = MiniOptions.of(Array(), Array(), Array())
    val setup = MiniSetup.of(sbt.internal.inc.CompileOutput(dir), options, "3.9.0", CompileOrder.Mixed, true, Array())
    AnalysisContents.create(sbt.internal.inc.Analysis.empty, setup)

  dir.test("AnalysisCache deletes the analysis file when writing it fails") { dir =>
    val file = Files.writeString(dir.resolve("analysis.zip"), "an older analysis")
    val analyses = AnalysisCache()
    intercept[IllegalStateException](analyses.write(file, unwritable))
    assert(!Files.exists(file), "the old analysis no longer matches the class files")
    assertEquals(analyses.read(file), None)
  }

  dir.test("AnalysisCache never returns an analysis whose rewrite failed, even if it cannot be deleted") { dir =>
    val file = dir.resolve("analysis.zip")
    val marker = AnalysisCache.marker(Validation.canonical(file))
    // Another target's file, with the name a marker next to analysis.zip might have had.
    val neighbour = Files.writeString(dir.resolve("analysis.zip.invalid"), "another target")
    val first = AnalysisCache(delete = _ => ())
    first.write(file, emptyAnalysis(dir))
    assert(first.read(file).isDefined)

    val undeletable = AnalysisCache(delete = _ => throw java.io.IOException("cannot delete"))
    assert(undeletable.read(file).isDefined) // now cached
    val e = intercept[IllegalStateException](undeletable.write(file, unwritable))
    assert(e.getSuppressed.exists(_.getMessage == "cannot delete"), e.getSuppressed.toList)
    assert(Files.exists(file), "the stale analysis is still there")
    assert(Files.exists(marker))
    assertEquals(undeletable.read(file), None)
    assertEquals(undeletable.read(dir.resolve(".").resolve("analysis.zip")), None)
    assertEquals(AnalysisCache().read(file), None, "a restarted worker must not read it either")

    undeletable.write(file, emptyAnalysis(dir))
    assert(undeletable.read(file).isDefined, "a successful write makes it readable again")
    assert(!Files.exists(marker))
    assert(AnalysisCache().read(file).isDefined)
    assertEquals(Files.readString(neighbour), "another target")
  }

  dir.test("AnalysisCache keeps a sibling's marker, and the marker directory, when its own write succeeds") { dir =>
    val a = dir.resolve("a.zip")
    val b = dir.resolve("b.zip")
    val analyses = AnalysisCache(delete = _ => throw java.io.IOException("cannot delete"))
    analyses.write(a, emptyAnalysis(dir))
    analyses.write(b, emptyAnalysis(dir))
    val markerA = AnalysisCache.marker(Validation.canonical(a))
    val markerB = AnalysisCache.marker(Validation.canonical(b))

    intercept[IllegalStateException](analyses.write(b, unwritable))
    analyses.write(b, emptyAnalysis(dir))
    assert(!Files.exists(markerB))
    // A failed write elsewhere in this directory may be between creating it and its marker.
    assert(Files.isDirectory(markerB.getParent), "a successful write leaves the marker directory")

    intercept[IllegalStateException](analyses.write(a, unwritable))
    intercept[IllegalStateException](analyses.write(b, unwritable))
    analyses.write(b, emptyAnalysis(dir))
    assert(Files.exists(markerA), "a's marker survives b's successful write")
    assertEquals(AnalysisCache().read(a), None)
    assert(AnalysisCache().read(b).isDefined)
  }

  dir.test("AnalysisCache forgets an analysis whose rewrite failed in a read-only directory") { dir =>
    import java.nio.file.attribute.PosixFilePermissions.fromString
    val sub = Files.createDirectory(dir.resolve("ro"))
    val file = sub.resolve("analysis.zip")
    val analyses = AnalysisCache()
    analyses.write(file, emptyAnalysis(dir))
    assert(analyses.read(file).isDefined)
    Files.setPosixFilePermissions(sub, fromString("r-xr-xr-x"))
    try
      // Root, or CAP_DAC_OVERRIDE, can still write to it; the injected-failure test covers that case.
      assume(!Files.isWritable(sub), "the directory is writable despite its permissions")
      intercept[IllegalStateException](analyses.write(file, unwritable))
      assert(Files.exists(file) && !Files.exists(AnalysisCache.marker(Validation.canonical(file))), "neither deleted nor marked")
      assertEquals(analyses.read(file), None)
    finally Files.setPosixFilePermissions(sub, fromString("rwxr-xr-x"))
  }

  extension [A](a: A) private def tap(f: A => Unit): A = { f(a); a }
