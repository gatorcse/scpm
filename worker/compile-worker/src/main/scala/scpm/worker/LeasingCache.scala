package scpm.worker

/**
 * A bounded, least-recently-used cache whose values are leased while in use. A value that falls
 * out of the cache (capacity, or [[evict]]) is closed once its last lease ends, so a compile that
 * is still running never has its class loaders closed underneath it.
 */
final class LeasingCache[K, V](capacity: Int, create: K => V, close: V => Unit):
  private final class Entry(val value: V):
    var leases = 0
    var retired = false

  private val entries = java.util.LinkedHashMap[K, Entry](16, 0.75f, true)

  def use[A](key: K)(f: V => A): A =
    val entry = acquire(key)
    try f(entry.value)
    finally release(entry)

  def evict(key: K): Unit = synchronized(Option(entries.remove(key)).foreach(retire))

  def size: Int = synchronized(entries.size)

  private def acquire(key: K): Entry = synchronized {
    val entry = Option(entries.get(key)).getOrElse {
      val created = Entry(create(key))
      entries.put(key, created)
      val eldest = entries.entrySet.iterator
      while entries.size > capacity do
        val victim = eldest.next().getValue
        eldest.remove()
        retire(victim)
      created
    }
    entry.leases += 1
    entry
  }

  private def release(entry: Entry): Unit = synchronized {
    entry.leases -= 1
    if entry.retired && entry.leases == 0 then close(entry.value)
  }

  private def retire(entry: Entry): Unit =
    entry.retired = true
    if entry.leases == 0 then close(entry.value)
