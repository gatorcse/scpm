package app

@main def Main(args: String*): Unit =
  println(javalib.Shout.shout(util.Greeting.hello(args.toList)))
