package scpm.worker

import sbt.internal.inc.consistent.ConsistentFileAnalysisStore
import xsbti.compile.AnalysisContents
import xsbti.compile.analysis.ReadWriteMappers

import java.io.IOException
import java.nio.file.attribute.{ BasicFileAttributes, FileTime }
import java.nio.file.{ Files, NoSuchFileException, Path }
import scala.jdk.OptionConverters.*

/** Zinc analysis files, read through a [[FileCache]] so a warm worker does not deserialize them again. */
final class AnalysisCache(capacity: Int = 64):
  private val cache = FileCache[AnalysisContents](capacity, file => store(file).get.toScala)

  def read(file: Path): Option[AnalysisContents] = cache.read(file)

  /**
   * Writes `contents` to `file`. If that fails, `file` is deleted: the class files it describes
   * are already in place, so an older analysis there would not match them. With none, the next
   * compile starts from an empty classes directory.
   */
  def write(file: Path, contents: AnalysisContents): Unit =
    try store(file).set(contents)
    catch
      case e: Throwable =>
        try Files.deleteIfExists(file)
        catch case d: IOException => e.addSuppressed(d)
        throw e
    cache.put(file, contents)

  private def store(file: Path) =
    ConsistentFileAnalysisStore.binary(file.toFile, ReadWriteMappers.getEmptyMappers)

/**
 * The parsed contents of up to `maxEntries` files, least recently used first out. An entry is
 * reused only while the file is the one it came from: same modification time, size and file key
 * (inode), so a file that was deleted, replaced or rewritten is read again.
 */
final class FileCache[A](maxEntries: Int, load: Path => Option[A]):
  private final case class Version(modified: FileTime, size: Long, key: Option[AnyRef])
  private final case class Entry(version: Version, value: A)

  private val entries = new java.util.LinkedHashMap[Path, Entry](16, 0.75f, true):
    override def removeEldestEntry(eldest: java.util.Map.Entry[Path, Entry]): Boolean = size > maxEntries

  def read(file: Path): Option[A] =
    version(file) match
      case None =>
        synchronized(entries.remove(file))
        None
      case Some(current) =>
        synchronized(Option(entries.get(file))).filter(_.version == current).map(_.value).orElse {
          val value = load(file)
          value.foreach(v => synchronized(entries.put(file, Entry(current, v))))
          value
        }

  /** Records `value` as the contents of `file`, which was just written. */
  def put(file: Path, value: A): Unit =
    version(file).foreach(v => synchronized(entries.put(file, Entry(v, value))))

  private def version(file: Path): Option[Version] =
    try
      val attrs = Files.readAttributes(file, classOf[BasicFileAttributes])
      Some(Version(attrs.lastModifiedTime, attrs.size, Option(attrs.fileKey)))
    catch case _: NoSuchFileException => None
