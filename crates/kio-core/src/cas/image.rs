use super::*;

/// Image-only CAS authority. Construction retains the original private root;
/// namespaces remain lazy so quota rejection has no filesystem side effects.
#[derive(Debug, Clone)]
pub struct ImageStore {
    kio: Arc<File>,
    directories: Arc<Mutex<ImageDirectories>>,
}

#[derive(Debug)]
struct ImageDirectories {
    objects: Option<Arc<File>>,
    image: Option<Arc<File>>,
}

impl ObjectStore {
    pub fn image_store(&self) -> Result<ImageStore> {
        let bound = self.bound.as_ref().ok_or_else(|| {
            KioError::invalid_usage("image persistence requires a retained ObjectStore")
        })?;
        ensure_bound_directory(&bound.kio, Path::new(".kio"))?;
        if !same_bound_directory(&bound_open_dir(&bound.kio, "objects")?, &bound.objects)? {
            return Err(bound_stage_corrupt(
                "retained objects namespace was replaced",
            ));
        }
        let mut image = bound
            .image
            .lock()
            .map_err(|_| KioError::io("image directory lock poisoned", "image"))?
            .clone();
        if image.is_none() {
            image = match bound_open_dir(&bound.objects, "image") {
                Ok(directory) => Some(Arc::new(directory)),
                Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => None,
                Err(error) => return Err(error),
            };
        }
        Ok(ImageStore {
            kio: Arc::clone(&bound.kio),
            directories: Arc::new(Mutex::new(ImageDirectories {
                objects: Some(Arc::clone(&bound.objects)),
                image,
            })),
        })
    }
}

impl ImageStore {
    pub fn from_bound_kio(kio: &File) -> Result<Self> {
        ensure_bound_directory(kio, Path::new(".kio"))?;
        let retained = kio
            .try_clone()
            .map_err(|e| KioError::io(e.to_string(), ".kio"))?;
        let kio = cas_semantics::retain_bound_kio(retained)?;
        let optional = |parent: &File, leaf: &str| match bound_open_dir(parent, leaf) {
            Ok(directory) => Ok(Some(Arc::new(directory))),
            Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => Ok(None),
            Err(error) => Err(error),
        };
        let objects = optional(&kio, "objects")?;
        let image = objects
            .as_ref()
            .map(|objects| optional(objects, "image"))
            .transpose()?
            .flatten();
        Ok(Self {
            kio,
            directories: Arc::new(Mutex::new(ImageDirectories { objects, image })),
        })
    }

    fn fanout(&self, hash: &str, create: bool) -> Result<(File, String)> {
        ensure_bound_directory(&self.kio, Path::new(".kio"))?;
        let digest = hash_path_component(hash)?;
        let mut directories = self
            .directories
            .lock()
            .map_err(|_| KioError::io("image directory lock poisoned", "image"))?;
        if directories.objects.is_none() {
            directories.objects = Some(Arc::new(bound_open_or_create_dir(
                &self.kio, "objects", create,
            )?));
        }
        let objects = directories
            .objects
            .as_ref()
            .expect("retained objects namespace");
        if !same_bound_directory(&bound_open_dir(&self.kio, "objects")?, objects)? {
            return Err(bound_stage_corrupt(
                "retained objects namespace was replaced",
            ));
        }
        ensure_bound_directory(objects, Path::new("objects"))?;
        if directories.image.is_none() {
            directories.image = Some(Arc::new(bound_open_or_create_dir(
                objects, "image", create,
            )?));
        }
        let image = directories
            .image
            .as_ref()
            .expect("retained image namespace");
        let objects = directories
            .objects
            .as_ref()
            .expect("retained objects namespace");
        if !same_bound_directory(&bound_open_dir(objects, "image")?, image)? {
            return Err(bound_stage_corrupt("retained image namespace was replaced"));
        }
        let first = bound_open_or_create_dir(image, &digest[..2], create)?;
        let second = bound_open_or_create_dir(&first, &digest[2..4], create)?;
        Ok((second, digest.to_owned()))
    }

    pub fn read(&self, hash: &str, max_bytes: u64) -> Result<Vec<u8>> {
        let (parent, leaf) = self.fanout(hash, false)?;
        bound_read_regular(&CasReadAccess::Unmetered, &parent, &leaf, max_bytes)
            .map(|(_, bytes)| bytes)
    }

    pub fn write(&self, bytes: &[u8]) -> Result<String> {
        let kind = ContentObjectKind::Image;
        if bytes.len() as u64 > kind.max_bytes() {
            return Err(content_object_size_error(
                kind,
                kind.max_bytes(),
                bytes.len() as u64,
            ));
        }
        let hash = hash_bytes(bytes);
        let (parent, leaf) = self.fanout(&hash, true)?;
        write_bound_content_at(&parent, &leaf, kind, &hash, bytes)?;
        Ok(hash)
    }
}

pub(super) fn write_bound_content_at(
    parent: &File,
    leaf: &str,
    kind: ContentObjectKind,
    hash: &str,
    bytes: &[u8],
) -> Result<()> {
    match bound_read_regular(&CasReadAccess::Unmetered, parent, leaf, kind.max_bytes()) {
        Ok((_, existing)) => {
            if hash_bytes(&existing) == hash && existing == bytes {
                return Ok(());
            }
            return Err(corrupt_object_error(
                Path::new(leaf),
                "CAS content bytes do not match existing object",
                hash,
                Some(&hash_bytes(&existing)),
            ));
        }
        Err(error) if error.error_code() == "KIO-E-STORE-NOT-FOUND-001" => {}
        Err(error) => return Err(error),
    }
    let (temp_name, mut temp) = bound_create_temp(parent)?;
    let result = (|| {
        temp.write_all(bytes).kio_io(Path::new(&temp_name))?;
        temp.sync_all().kio_io(Path::new(&temp_name))?;
        drop(temp);
        bound_publish_content(parent, &temp_name, leaf, kind, hash, bytes)
    })();
    if result.is_err() {
        let _ = bound_remove(parent, &temp_name);
    }
    result
}
