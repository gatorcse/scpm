package scpm.worker

import sbt.internal.inc.caching.ClasspathCache
import sbt.internal.inc.{ APIs, Locate, NoopExternalLookup }
import xsbti.api.AnalyzedClass
import xsbti.compile.{ CompileAnalysis, DefinesClass, FileHash, PerClasspathEntryLookup }
import xsbti.{ FileConverter, VirtualFile, VirtualFileRef }

import java.nio.file.attribute.BasicFileAttributes
import java.nio.file.{ Files, Path }
import java.util.Optional
import scala.jdk.CollectionConverters.*
import scala.jdk.OptionConverters.*
import scala.util.Using
import scala.util.hashing.MurmurHash3

/**
 * Gives Zinc the analyses of upstream modules, keyed by their classes directories, the way sbt
 * does for inter-project dependencies. Classes found in an upstream analysis become external
 * dependencies, so an API change or a removed class upstream invalidates the sources using it.
 */
final class UpstreamAnalyses(upstream: Map[Path, Path], analyses: AnalysisCache, converter: FileConverter)
    extends PerClasspathEntryLookup:
  def analysis(classpathEntry: VirtualFile): Optional[CompileAnalysis] =
    val classesDir = converter.toPath(classpathEntry)
    upstream
      .get(classesDir)
      // A wiped classes directory makes its analysis meaningless.
      .filter(_ => Files.isDirectory(classesDir))
      .flatMap(analyses.read)
      .map(_.getAnalysis)
      .toJava

  def definesClass(classpathEntry: VirtualFile): DefinesClass = Locate.definesClass(classpathEntry)

/**
 * Zinc only looks for changed library classes when the classpath hash changes, and it hashes a
 * directory as if it were empty. This hashes directories by the names, sizes and modification
 * times of their files, so that an upstream classes directory without an analysis still
 * invalidates dependents when its class files change or disappear.
 */
final class ClassDirectoryHashing(converter: FileConverter) extends NoopExternalLookup:
  // Let Zinc find external classes through the upstream analyses.
  override def lookupAnalyzedClass(binaryClassName: String, file: Option[VirtualFileRef]): Option[AnalyzedClass] =
    Some(APIs.emptyAnalyzedClass)

  override def hashClasspath(classpath: Array[VirtualFile]): Optional[Array[FileHash]] =
    Optional.of(classpath.map { entry =>
      val path = converter.toPath(entry)
      if Files.isDirectory(path) then FileHash.of(path, ClassDirectoryHashing.hashDirectory(path))
      else ClasspathCache.hashClasspath(Seq(path)).head
    })

object ClassDirectoryHashing:
  def hashDirectory(dir: Path): Int =
    Using.resource(Files.walk(dir)) { files =>
      val hashes = files.iterator.asScala
        .filter(Files.isRegularFile(_))
        .map { file =>
          val attrs = Files.readAttributes(file, classOf[BasicFileAttributes])
          (dir.relativize(file).toString, attrs.size, attrs.lastModifiedTime.toMillis).hashCode
        }
      MurmurHash3.unorderedHash(hashes.toSeq)
    }
