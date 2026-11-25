package scpm.worker

import sbt.internal.inc.{ AnalyzingCompiler, ScalaInstance, ZincUtil }

import java.io.File
import java.net.URLClassLoader
import java.nio.file.Path
import java.util.concurrent.ConcurrentHashMap

/**
 * Scala compilers, cached for the life of the worker. Keeping the compiler class loaders alive
 * between requests is what makes a warm worker fast: the JIT-compiled compiler stays hot.
 */
final class ScalaCompilers:
  import ScalaCompilers.Key

  private val cache = ConcurrentHashMap[Key, AnalyzingCompiler]()

  def get(version: String, compilerJars: Seq[Path], libraryJars: Seq[Path], bridge: Path): AnalyzingCompiler =
    cache.computeIfAbsent(
      Key(version, compilerJars, libraryJars, bridge),
      key => ZincUtil.scalaCompiler(ScalaCompilers.instance(key), key.bridge)
    )

object ScalaCompilers:
  private final case class Key(version: String, compilerJars: Seq[Path], libraryJars: Seq[Path], bridge: Path)

  /**
   * Loader layering: platform classes + `xsbti` <- scala library <- scala compiler.
   *
   * The compiler and the bridge must see the same `xsbti` interfaces as Zinc, so `xsbti.*` always
   * comes from the worker's own loader, even when the compiler's classpath has its own
   * compiler-interface jar (Scala 3 does). Nothing else of the worker leaks into the compiler.
   */
  private def instance(key: Key): ScalaInstance =
    val libraryJars = key.libraryJars.map(_.toFile).toArray
    val compilerJars = key.compilerJars.map(_.toFile).toArray
    val libraryLoader = URLClassLoader(urls(libraryJars), XsbtiLoader)
    val compilerLoader = URLClassLoader(urls(compilerJars.filterNot(libraryJars.contains)), libraryLoader)
    new ScalaInstance(
      version = key.version,
      loader = compilerLoader,
      loaderCompilerOnly = compilerLoader,
      loaderLibraryOnly = libraryLoader,
      libraryJars = libraryJars,
      compilerJars = compilerJars,
      allJars = (libraryJars ++ compilerJars).distinct,
      explicitActual = Some(key.version)
    )

  private def urls(files: Array[File]) = files.map(_.toURI.toURL)

  private object XsbtiLoader extends ClassLoader("scpm-xsbti", ClassLoader.getPlatformClassLoader):
    private val worker = classOf[xsbti.Reporter].getClassLoader

    override def loadClass(name: String, resolve: Boolean): Class[?] =
      if name.startsWith("xsbti.") then worker.loadClass(name)
      else super.loadClass(name, resolve)
