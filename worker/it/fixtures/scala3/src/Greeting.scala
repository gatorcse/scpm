package hello

object Greeting:
  def greet(name: String): String = s"Hello, $name!"
  def default(): String = greet("world")
