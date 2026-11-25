package scpm.worker

import sbt.internal.inc.consistent.ConsistentFileAnalysisStore
import xsbti.compile.AnalysisContents
import xsbti.compile.analysis.ReadWriteMappers

import java.nio.file.attribute.FileTime
import java.nio.file.{ Files, Path }
import java.util.concurrent.ConcurrentHashMap
import scala.jdk.OptionConverters.*

/**
 * Zinc analysis files, with the last analysis read or written kept in memory so that a warm
 * worker does not deserialize it again. An entry is reused only while the file on disk is the
 * one it came from, so deleting or replacing the file (e.g. a remote cache restore) is honoured.
 */
final class AnalysisCache:
  private final case class Entry(modified: FileTime, contents: AnalysisContents)
  private val entries = ConcurrentHashMap[Path, Entry]()

  def read(file: Path): Option[AnalysisContents] =
    if !Files.exists(file) then
      entries.remove(file)
      None
    else
      val modified = Files.getLastModifiedTime(file)
      Option(entries.get(file)).filter(_.modified == modified).map(_.contents).orElse {
        val contents = store(file).get.toScala
        contents.foreach(c => entries.put(file, Entry(modified, c)))
        contents
      }

  def write(file: Path, contents: AnalysisContents): Unit =
    store(file).set(contents)
    entries.put(file, Entry(Files.getLastModifiedTime(file), contents))

  private def store(file: Path) =
    ConsistentFileAnalysisStore.binary(file.toFile, ReadWriteMappers.getEmptyMappers)
