package scpm.testrunner;

import java.io.IOException;
import java.io.UncheckedIOException;
import java.lang.annotation.Annotation;
import java.lang.reflect.Method;
import java.lang.reflect.Modifier;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.stream.Stream;
import sbt.testing.AnnotatedFingerprint;
import sbt.testing.Fingerprint;
import sbt.testing.SubclassFingerprint;

/** Finds test classes by loading every compiled class and matching it against fingerprints. */
final class Discovery {
  private final ClassLoader loader;
  private final Map<String, Optional<Class<?>>> loaded = new HashMap<>();

  Discovery(ClassLoader loader) {
    this.loader = loader;
  }

  /** Binary names of all classes under the given class directories, in a stable order. */
  static List<String> classNames(List<String> classesDirs) {
    List<String> names = new ArrayList<>();
    for (String dir : classesDirs) {
      Path root = Path.of(dir);
      if (!Files.isDirectory(root)) continue;
      try (Stream<Path> files = Files.walk(root)) {
        files
            .map(root::relativize)
            .map(Path::toString)
            .filter(f -> f.endsWith(".class") && !f.equals("module-info.class"))
            .sorted()
            .forEach(f -> names.add(f.substring(0, f.length() - 6).replace('/', '.').replace('\\', '.')));
      } catch (IOException e) {
        throw new UncheckedIOException(e);
      }
    }
    return names;
  }

  /** The first fingerprint the class matches, if any. */
  Optional<Fingerprint> match(String className, Fingerprint[] fingerprints) {
    Optional<Class<?>> loadedClass = load(className);
    if (loadedClass.isEmpty()) return Optional.empty();
    Class<?> cls = loadedClass.get();
    if (cls.isInterface() || Modifier.isAbstract(cls.getModifiers())) return Optional.empty();
    boolean isModule = className.endsWith("$");
    for (Fingerprint fp : fingerprints) {
      if (fp instanceof SubclassFingerprint sub
          && sub.isModule() == isModule
          && isSubclass(cls, sub.superclassName())
          // An object is instantiated through its MODULE$ field, never its constructor.
          && (isModule || !sub.requireNoArgConstructor() || hasNoArgConstructor(cls))) {
        return Optional.of(fp);
      }
      if (fp instanceof AnnotatedFingerprint ann
          && ann.isModule() == isModule
          && isAnnotated(cls, ann.annotationName())) {
        return Optional.of(fp);
      }
    }
    return Optional.empty();
  }

  /** The name frameworks expect in a TaskDef: Scala objects without their trailing {@code $}. */
  static String testName(String className) {
    return className.endsWith("$") ? className.substring(0, className.length() - 1) : className;
  }

  private boolean isSubclass(Class<?> cls, String superclassName) {
    return load(superclassName).map(sup -> sup != cls && sup.isAssignableFrom(cls)).orElse(false);
  }

  private boolean isAnnotated(Class<?> cls, String annotationName) {
    Optional<Class<?>> annotation = load(annotationName).filter(Class::isAnnotation);
    if (annotation.isEmpty()) return false;
    @SuppressWarnings("unchecked")
    Class<? extends Annotation> a = (Class<? extends Annotation>) annotation.get();
    if (cls.isAnnotationPresent(a)) return true;
    try {
      for (Method m : cls.getMethods()) if (m.isAnnotationPresent(a)) return true;
    } catch (LinkageError e) {
      return false;
    }
    return false;
  }

  private static boolean hasNoArgConstructor(Class<?> cls) {
    try {
      cls.getConstructor();
      return true;
    } catch (NoSuchMethodException | LinkageError e) {
      return false;
    }
  }

  /** Loads without initializing; classes that fail to link are simply not tests. */
  private Optional<Class<?>> load(String name) {
    return loaded.computeIfAbsent(
        name,
        n -> {
          try {
            return Optional.of(Class.forName(n, false, loader));
          } catch (ClassNotFoundException | LinkageError e) {
            return Optional.empty();
          }
        });
  }
}
