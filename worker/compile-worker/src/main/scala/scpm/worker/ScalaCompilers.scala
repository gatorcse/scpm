package scpm.worker

import sbt.internal.inc.classpath.ClassLoaderCache
import sbt.internal.inc.{ AnalyzingCompiler, ScalaInstance, ZincUtil }
import xsbti.compile.ClasspathOptionsUtil

import java.io.File
import java.net.URLClassLoader
import java.nio.file.Path

/** Everything that identifies a Scala compiler: version, jars and compiler bridge. */
final case class Toolchain(version: String, compilerJars: Seq[Path], libraryJars: Seq[Path], bridge: Path):
  def isScala3: Boolean = version.startsWith("3.")

object Toolchain:
  def of(params: CompileParams): Toolchain =
    Toolchain(
      params.scalaVersion,
      params.scalaCompilerClasspath.map(Path.of(_)),
      params.scalaLibraryClasspath.map(Path.of(_)),
      Path.of(params.compilerBridge)
    )

/**
 * Scala compilers, kept between requests: keeping the compiler class loaders alive is what makes
 * a warm worker fast, because the JIT-compiled compiler stays hot. At most `capacity` toolchains
 * are kept; the class loaders of an evicted one are closed once no compile uses them.
 */
final class ScalaCompilers(capacity: Int = 8):
  import ScalaCompilers.{ Loaded, load }

  private val cache = LeasingCache[Toolchain, Loaded](capacity, load, _.close())

  def use[A](toolchain: Toolchain)(f: AnalyzingCompiler => A): A =
    cache.use(toolchain)(loaded => f(loaded.compiler))

  /** Forgets a toolchain whose class loaders turned out to be broken. */
  def evict(toolchain: Toolchain): Unit = cache.evict(toolchain)

object ScalaCompilers:
  private final class Loaded(val compiler: AnalyzingCompiler, loaders: Seq[AutoCloseable]):
    def close(): Unit = loaders.foreach(_.close())

  /**
   * Loader layering: platform classes + `xsbti` <- scala library <- scala compiler.
   *
   * The compiler and the bridge must see the same `xsbti` interfaces as Zinc, so `xsbti.*` always
   * comes from the worker's own loader, even when the compiler's classpath has its own
   * compiler-interface jar (Scala 3 does). Nothing else of the worker leaks into the compiler.
   */
  private def load(t: Toolchain): Loaded =
    val libraryJars = t.libraryJars.map(_.toFile).toArray
    val compilerJars = t.compilerJars.map(_.toFile).toArray
    val libraryLoader = URLClassLoader(urls(libraryJars), XsbtiLoader)
    val compilerLoader = URLClassLoader(urls(compilerJars.filterNot(libraryJars.contains)), libraryLoader)
    val instance = new ScalaInstance(
      version = t.version,
      loader = compilerLoader,
      loaderCompilerOnly = compilerLoader,
      loaderLibraryOnly = libraryLoader,
      libraryJars = libraryJars,
      compilerJars = compilerJars,
      allJars = (libraryJars ++ compilerJars).distinct,
      explicitActual = Some(t.version)
    )
    val bridgeLoaders = ClassLoaderCache(URLClassLoader(Array()))
    val compiler = AnalyzingCompiler(
      instance,
      ZincUtil.constantBridgeProvider(instance, t.bridge),
      ClasspathOptionsUtil.noboot(t.version),
      _ => (),
      Some(bridgeLoaders)
    )
    Loaded(compiler, Seq(bridgeLoaders, compilerLoader, libraryLoader))

  private def urls(files: Array[File]) = files.map(_.toURI.toURL)

  private object XsbtiLoader extends ClassLoader("scpm-xsbti", ClassLoader.getPlatformClassLoader):
    private val worker = classOf[xsbti.Reporter].getClassLoader

    override def loadClass(name: String, resolve: Boolean): Class[?] =
      if name.startsWith("xsbti.") then worker.loadClass(name)
      else super.loadClass(name, resolve)
